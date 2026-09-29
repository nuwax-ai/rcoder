//! 对比引擎：协议头判定、JSON 差异、zip 语义对比与 diff 归一化。

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::assertions::*;
use crate::report::*;
use crate::snapshot::*;
use crate::types::*;
use crate::util::*;

/// Outcome of comparing one response header across implementations under its real
/// protocol semantics, validating each side's own value before declaring the two
/// equivalent. Raw values stay in the report either way.
pub(crate) enum HeaderVerdict {
    Equivalent,
    Different,
    Invalid { side: &'static str, reason: String },
}

/// Protocol-level equivalence for headers whose representations legitimately vary
/// across implementations:
/// - `content-type`: identical media type with the charset stated explicitly
///   (`; charset=utf-8`) vs implicitly (JSON's default is UTF-8) is equivalent;
///   different media types or different charsets are not.
/// - `etag`: entity tags are opaque; two well-formed tags are equivalent. On static
///   representations, presence must match (validators are part of that contract).
///   Elsewhere — including error responses — the TypeScript side emits Express
///   framework defaults that the Rust side omits, and the conditional-request
///   contract is enforced by dedicated scenarios.
/// - `content-length`: each side must match its own transferred body; then the two
///   are equivalent by derivation. A mismatch with its own body is a side failure.
/// - `last-modified`: on static content both values are the per-side fixture file
///   mtimes; two well-formed HTTP dates are equivalent there.
///
/// All other headers (and every unlisted case above) compare strictly.
pub(crate) fn header_protocol_verdict(
    header: &str,
    rust_value: Option<&str>,
    ts_value: Option<&str>,
    static_representation: bool,
    rust_body_len: usize,
    ts_body_len: usize,
) -> HeaderVerdict {
    match header {
        "content-type" => match (rust_value, ts_value) {
            (Some(rust), Some(ts)) => {
                match (media_type_and_charset(rust), media_type_and_charset(ts)) {
                    (Some((rust_type, rust_charset)), Some((ts_type, ts_charset))) => {
                        if !rust_type.eq_ignore_ascii_case(&ts_type) {
                            HeaderVerdict::Different
                        } else if rust_charset.eq_ignore_ascii_case(&ts_charset) {
                            HeaderVerdict::Equivalent
                        } else {
                            HeaderVerdict::Different
                        }
                    }
                    _ => HeaderVerdict::Invalid {
                        side: if media_type_and_charset(rust).is_none() {
                            "rust"
                        } else {
                            "typescript"
                        },
                        reason: "content-type must be a media type with optional parameters"
                            .to_string(),
                    },
                }
            }
            _ => HeaderVerdict::Different,
        },
        "etag" => match (rust_value, ts_value) {
            (Some(rust), Some(ts)) => match (entity_tag_shape(rust), entity_tag_shape(ts)) {
                (true, true) => HeaderVerdict::Equivalent,
                (false, _) | (_, false) => HeaderVerdict::Invalid {
                    side: if !entity_tag_shape(rust) {
                        "rust"
                    } else {
                        "typescript"
                    },
                    reason: "etag must be a well-formed entity tag (W/\"…\" or \"…\")".to_string(),
                },
            },
            (None, None) => HeaderVerdict::Equivalent,
            // Static representations serve validators on both sides; a presence
            // mismatch there is material. Elsewhere Express adds weak etags by default.
            _ if static_representation => HeaderVerdict::Different,
            _ => HeaderVerdict::Equivalent,
        },
        "content-length" => {
            let rust_len = rust_value.and_then(|value| value.parse::<usize>().ok());
            let ts_len = ts_value.and_then(|value| value.parse::<usize>().ok());
            match (rust_value.is_some(), ts_value.is_some(), rust_len, ts_len) {
                (false, false, _, _) => HeaderVerdict::Equivalent,
                (true, true, Some(rust_len), Some(ts_len)) => {
                    if rust_len != rust_body_len {
                        HeaderVerdict::Invalid {
                            side: "rust",
                            reason: format!(
                                "content-length {rust_len} does not match the {rust_body_len}-byte body"
                            ),
                        }
                    } else if ts_len != ts_body_len {
                        HeaderVerdict::Invalid {
                            side: "typescript",
                            reason: format!(
                                "content-length {ts_len} does not match the {ts_body_len}-byte body"
                            ),
                        }
                    } else {
                        HeaderVerdict::Equivalent
                    }
                }
                (true, true, _, _) => HeaderVerdict::Invalid {
                    side: if rust_len.is_none() {
                        "rust"
                    } else {
                        "typescript"
                    },
                    reason: "content-length must be a non-negative integer".to_string(),
                },
                _ => HeaderVerdict::Different,
            }
        }
        "last-modified" => match (rust_value, ts_value) {
            (Some(rust), Some(ts)) if static_representation => {
                match (http_date_shape(rust), http_date_shape(ts)) {
                    (true, true) => HeaderVerdict::Equivalent,
                    (false, _) | (_, false) => HeaderVerdict::Invalid {
                        side: if !http_date_shape(rust) {
                            "rust"
                        } else {
                            "typescript"
                        },
                        reason: "last-modified must be an HTTP-date".to_string(),
                    },
                }
            }
            _ => HeaderVerdict::Different,
        },
        _ => HeaderVerdict::Different,
    }
}

/// Split `type/subtype; key=value` into the lowercase media type and an effective
/// charset (`utf-8` when absent, matching JSON's and the implementations' default).
pub(crate) fn media_type_and_charset(value: &str) -> Option<(String, String)> {
    let (media_type, parameters) = match value.split_once(';') {
        Some((media_type, parameters)) => (media_type, parameters),
        None => (value, ""),
    };
    let media_type = media_type.trim();
    let (major, minor) = media_type.split_once('/')?;
    if major.is_empty() || minor.is_empty() || minor.contains('/') {
        return None;
    }
    let charset = parameters
        .split(';')
        .filter_map(|parameter| parameter.trim().split_once('='))
        .find(|(key, _)| key.eq_ignore_ascii_case("charset"))
        .map(|(_, value)| value.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "utf-8".to_string());
    Some((media_type.to_ascii_lowercase(), charset))
}

/// `W/"opaque"` or `"opaque"` with non-empty opaque text and no inner quotes.
pub(crate) fn entity_tag_shape(value: &str) -> bool {
    let body = value.strip_prefix("W/").unwrap_or(value);
    let Some(inner) = body
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return false;
    };
    !inner.is_empty() && !inner.contains('"')
}

/// IMF-fixdate shape (`Sun, 06 Nov 1994 08:49:37 GMT`).
pub(crate) fn http_date_shape(value: &str) -> bool {
    chrono::DateTime::parse_from_rfc2822(value).is_ok()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn compare_exchange(
    case: &str,
    rust: &Exchange,
    ts: &Exchange,
    health_probe: bool,
    expected_status: ExpectedStatus,
    normalized_paths: &[String],
    normalized_headers: &[String],
    static_representation: bool,
) -> Vec<Difference> {
    let mut differences = Vec::new();
    if rust.transport_error.is_some() || ts.transport_error.is_some() {
        differences.push(Difference {
            case: case.to_string(),
            path: "/transport".into(),
            kind: "transport_error".into(),
            rust_value: rust.transport_error.as_ref().map(|error| json!(error)),
            ts_value: ts.transport_error.as_ref().map(|error| json!(error)),
            accepted: false,
            reason: None,
        });
        return differences;
    }
    for (side, exchange) in [("rust", rust), ("typescript", ts)] {
        if !expected_status.matches(exchange.status) {
            differences.push(diff(
                case,
                &format!("/assertions/{side}/http-status"),
                "assertion_failed",
                Some(json!({
                    "expected": expected_status.label(),
                    "actual": exchange.status.map(|status| status.as_u16()),
                })),
                None,
            ));
        }
    }
    if rust.status != ts.status {
        differences.push(diff(
            case,
            "/http/status",
            "value",
            rust.status.map(|status| json!(status.as_u16())),
            ts.status.map(|status| json!(status.as_u16())),
        ));
    }
    for header in [
        "content-type",
        "content-length",
        "content-range",
        "accept-ranges",
        "cache-control",
        "location",
        "etag",
        "last-modified",
        "x-file-size",
        "access-control-allow-origin",
        "access-control-allow-credentials",
        "access-control-expose-headers",
        "vary",
    ] {
        if normalized_headers
            .iter()
            .any(|normalized| normalized == header)
        {
            continue;
        }
        let rust_value = rust
            .headers
            .get(header)
            .and_then(|value| value.to_str().ok());
        let ts_value = ts.headers.get(header).and_then(|value| value.to_str().ok());
        if rust_value == ts_value {
            continue;
        }
        match header_protocol_verdict(
            header,
            rust_value,
            ts_value,
            static_representation,
            rust.body.len(),
            ts.body.len(),
        ) {
            HeaderVerdict::Equivalent => {}
            HeaderVerdict::Different => {
                differences.push(diff(
                    case,
                    &format!("/http/headers/{header}"),
                    "value",
                    rust_value.map(|value| json!(value)),
                    ts_value.map(|value| json!(value)),
                ));
            }
            HeaderVerdict::Invalid { side, reason } => {
                differences.push(assertion_difference(
                    case,
                    &format!("/assertions/{side}/headers/{header}"),
                    reason,
                ));
            }
        }
    }

    if rust.status.is_none() || ts.status.is_none() {
        return differences;
    }
    if is_zip_exchange(rust) || is_zip_exchange(ts) {
        let rust_entries = zip_semantic_entries(&rust.body);
        let ts_entries = zip_semantic_entries(&ts.body);
        for (side, parsed) in [("rust", &rust_entries), ("typescript", &ts_entries)] {
            if let Err(error) = parsed {
                differences.push(diff(
                    case,
                    &format!("/assertions/{side}/valid-zip"),
                    "assertion_failed",
                    Some(json!(format!(
                        "response declared as ZIP but could not be parsed: {error:#}"
                    ))),
                    None,
                ));
            }
        }
        if let (Ok(rust_entries), Ok(ts_entries)) = (rust_entries, ts_entries) {
            json_differences(
                case,
                "/zip",
                &rust_entries,
                &ts_entries,
                normalized_paths,
                &mut differences,
            );
        }
        return differences;
    }
    let rust_json = serde_json::from_slice::<Value>(&rust.body).ok();
    let ts_json = serde_json::from_slice::<Value>(&ts.body).ok();
    if health_probe {
        let rust_status = rust_json.as_ref().and_then(|value| value.get("status"));
        let ts_status = ts_json.as_ref().and_then(|value| value.get("status"));
        if rust_status != ts_status || rust_status.is_none() || ts_status.is_none() {
            differences.push(diff(
                case,
                "/body/status",
                "value",
                rust_status
                    .cloned()
                    .or_else(|| Some(json!({"$missing": true}))),
                ts_status
                    .cloned()
                    .or_else(|| Some(json!({"$missing": true}))),
            ));
        }
    } else if let (Some(rust_json), Some(ts_json)) = (rust_json, ts_json) {
        json_differences(
            case,
            "",
            &rust_json,
            &ts_json,
            normalized_paths,
            &mut differences,
        );
    } else if rust.body != ts.body {
        differences.push(diff(
            case,
            "/body",
            "bytes",
            Some(json!({"bytes":rust.body.len(),"sha256":sha256(&rust.body)})),
            Some(json!({"bytes":ts.body.len(),"sha256":sha256(&ts.body)})),
        ));
    }
    differences
}

pub(crate) fn is_zip_exchange(exchange: &Exchange) -> bool {
    exchange
        .headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("zip"))
        || exchange.body.starts_with(b"PK\x03\x04")
        || exchange.body.starts_with(b"PK\x05\x06")
}

pub(crate) fn zip_semantic_entries(bytes: &[u8]) -> Result<Value> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("open ZIP archive")?;
    if archive.len() > ZIP_ENTRY_COUNT_LIMIT {
        bail!(
            "ZIP contains {} entries, above the comparison limit {ZIP_ENTRY_COUNT_LIMIT}",
            archive.len()
        );
    }
    let mut total_size = 0_u64;
    let mut entries = BTreeMap::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("read ZIP entry {index}"))?;
        let name = entry.name().replace('\\', "/");
        let declared_size = entry.size();
        if declared_size > ZIP_ENTRY_SIZE_LIMIT {
            bail!("ZIP entry {name:?} exceeds the per-entry comparison limit");
        }
        total_size = total_size
            .checked_add(declared_size)
            .context("sum ZIP uncompressed sizes")?;
        if total_size > ZIP_TOTAL_SIZE_LIMIT {
            bail!("ZIP exceeds the total uncompressed comparison limit");
        }
        let is_dir = entry.is_dir();
        let mode = entry.unix_mode();
        let mut content = Vec::with_capacity(declared_size as usize);
        (&mut entry)
            .take(ZIP_ENTRY_SIZE_LIMIT + 1)
            .read_to_end(&mut content)
            .with_context(|| format!("decompress ZIP entry {name:?}"))?;
        if content.len() as u64 > ZIP_ENTRY_SIZE_LIMIT {
            bail!("ZIP entry {name:?} exceeds the per-entry comparison limit");
        }
        if content.len() as u64 != declared_size {
            bail!("ZIP entry {name:?} size did not match its directory record");
        }
        let is_symlink = mode.is_some_and(|mode| mode & 0o170000 == 0o120000);
        let kind = if is_dir {
            "directory"
        } else if is_symlink {
            "symlink"
        } else {
            "file"
        };
        let content_sha = if name == ".npmrc" {
            normalize_generated_npmrc(&content).unwrap_or_else(|| sha256(&content))
        } else {
            sha256(&content)
        };
        let semantic = json!({
            "kind": kind,
            "sha256": if kind == "directory" { None } else { Some(content_sha) },
            "size_bytes": if kind == "directory" { None } else { Some(content.len()) },
            "mode": mode.map(|mode| format!("{:04o}", mode & 0o7777)),
        });
        if entries.insert(name.clone(), semantic).is_some() {
            bail!("ZIP contains duplicate entry path {name:?}");
        }
    }
    // A nonempty directory can be materialized from its child paths during ZIP extraction, so an
    // explicit conventional 0755 directory record is representation-only. Keep empty directories
    // and non-default permission records: those can change the extracted workspace.
    let implied_directories = entries
        .keys()
        .flat_map(|path| {
            let normalized = path.trim_end_matches('/');
            if normalized.is_empty() {
                return Vec::new();
            }
            let components: Vec<_> = normalized.split('/').collect();
            let mut parents = vec![String::new()];
            let mut parent = String::new();
            for component in components.iter().take(components.len().saturating_sub(1)) {
                if !parent.is_empty() {
                    parent.push('/');
                }
                parent.push_str(component);
                parents.push(format!("{parent}/"));
            }
            parents
        })
        .collect::<BTreeSet<_>>();
    for path in implied_directories {
        let is_conventional_nonempty_directory = entries.get(&path).is_some_and(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some("directory")
                && entry.get("mode").and_then(Value::as_str) == Some("0755")
        });
        if is_conventional_nonempty_directory {
            entries.remove(&path);
        }
    }
    serde_json::to_value(entries).context("serialize ZIP semantic entries")
}

pub(crate) fn validate_normalized_shape(
    case: &str,
    path: &str,
    rust: Option<&Value>,
    ts: Option<&Value>,
    output: &mut Vec<Difference>,
) {
    let same_shape = match (rust, ts) {
        (Some(a), Some(b)) => std::mem::discriminant(a) == std::mem::discriminant(b),
        _ => false,
    };
    if !same_shape {
        output.push(diff(
            case,
            path,
            "assertion_failed",
            rust.cloned(),
            ts.cloned(),
        ));
    }
}

pub(crate) fn json_differences(
    case: &str,
    path: &str,
    rust: &Value,
    ts: &Value,
    normalized_paths: &[String],
    output: &mut Vec<Difference>,
) {
    match (rust, ts) {
        (Value::Object(rust), Value::Object(ts)) => {
            let keys: BTreeSet<_> = rust.keys().chain(ts.keys()).collect();
            for key in keys {
                let child = format!("{path}/{}", json_pointer_escape(key));
                if normalized_paths
                    .iter()
                    .any(|normalized| normalized == &child)
                {
                    validate_normalized_shape(case, &child, rust.get(key), ts.get(key), output);
                    continue;
                }
                match (rust.get(key), ts.get(key)) {
                    (Some(rust), Some(ts)) => {
                        json_differences(case, &child, rust, ts, normalized_paths, output)
                    }
                    (Some(rust), None) => {
                        output.push(diff(case, &child, "rust_only", Some(rust.clone()), None))
                    }
                    (None, Some(ts)) => {
                        output.push(diff(case, &child, "ts_only", None, Some(ts.clone())))
                    }
                    (None, None) => {}
                }
            }
        }
        (Value::Array(rust), Value::Array(ts)) => {
            if let (Some(rust_by_name), Some(ts_by_name)) =
                (named_array_items(rust), named_array_items(ts))
            {
                let common_names = rust_by_name
                    .keys()
                    .filter(|name| ts_by_name.contains_key(**name))
                    .copied()
                    .collect::<BTreeSet<_>>();
                let rust_order = rust
                    .iter()
                    .filter_map(named_array_item_name)
                    .filter(|name| common_names.contains(name))
                    .collect::<Vec<_>>();
                let ts_order = ts
                    .iter()
                    .filter_map(named_array_item_name)
                    .filter(|name| common_names.contains(name))
                    .collect::<Vec<_>>();
                if rust_order != ts_order {
                    output.push(diff(
                        case,
                        &format!("{path}/$order"),
                        "array_order",
                        Some(json!(rust_order)),
                        Some(json!(ts_order)),
                    ));
                }

                let names = rust_by_name
                    .keys()
                    .chain(ts_by_name.keys())
                    .copied()
                    .collect::<BTreeSet<_>>();
                for name in names {
                    let child = format!("{path}/{}", json_pointer_escape(name));
                    match (rust_by_name.get(name), ts_by_name.get(name)) {
                        (Some(rust), Some(ts)) => {
                            json_differences(case, &child, rust, ts, normalized_paths, output)
                        }
                        (Some(rust), None) => output.push(diff(
                            case,
                            &child,
                            "rust_only",
                            Some((*rust).clone()),
                            None,
                        )),
                        (None, Some(ts)) => {
                            output.push(diff(case, &child, "ts_only", None, Some((*ts).clone())))
                        }
                        (None, None) => {}
                    }
                }
                return;
            }

            let max = rust.len().max(ts.len());
            for index in 0..max {
                let child = format!("{path}/{index}");
                if normalized_paths
                    .iter()
                    .any(|normalized| normalized == &child)
                {
                    validate_normalized_shape(case, &child, rust.get(index), ts.get(index), output);
                    continue;
                }
                match (rust.get(index), ts.get(index)) {
                    (Some(rust), Some(ts)) => {
                        json_differences(case, &child, rust, ts, normalized_paths, output)
                    }
                    (Some(rust), None) => {
                        output.push(diff(case, &child, "rust_only", Some(rust.clone()), None))
                    }
                    (None, Some(ts)) => {
                        output.push(diff(case, &child, "ts_only", None, Some(ts.clone())))
                    }
                    (None, None) => {}
                }
            }
        }
        (Value::String(rust), Value::String(ts)) if path == "/diff" => {
            if normalize_git_diff_hunk_headers(rust) != normalize_git_diff_hunk_headers(ts) {
                output.push(diff(
                    case,
                    path,
                    "value",
                    Some(Value::String(rust.clone())),
                    Some(Value::String(ts.clone())),
                ));
            }
        }
        _ if rust != ts => output.push(diff(
            case,
            path,
            "value",
            Some(rust.clone()),
            Some(ts.clone()),
        )),
        _ => {}
    }
}

/// Git-compatible diff producers may omit the line count when it is one, and they
/// disagree on the start line of an empty range at the top of a file (git writes
/// `-0,0`, gix writes `-1,0`; both mean "insertion before line 1"). Compare those
/// formatting conventions semantically while leaving all non-hunk text intact.
/// Git-compatible diff producers may omit the line count when it is one; that
/// omission is the only universally equivalent formatting difference. The observed
/// gix/system-Git divergence for empty top ranges (`-1,0` vs `-0,0`) is verified only
/// for whole-new-file hunks, so the 0/1 start equivalence is applied exclusively
/// inside `new file mode` blocks. Zero-line counts are always preserved: an empty
/// range (`-2,0`) is never conflated with an omitted one-line range (`-2`).
pub(crate) fn normalize_git_diff_hunk_headers(diff_text: &str) -> String {
    let mut in_new_file_block = false;
    diff_text
        .split_inclusive('\n')
        .map(|line| {
            let (body, newline) = line
                .strip_suffix('\n')
                .map_or((line, ""), |body| (body, "\n"));
            if body.starts_with("diff --git ") {
                in_new_file_block = false;
            } else if body.starts_with("new file mode") {
                in_new_file_block = true;
            }
            normalize_git_diff_hunk_header(body, in_new_file_block)
                .map(|normalized| format!("{normalized}{newline}"))
                .unwrap_or_else(|| line.to_string())
        })
        .collect()
}

pub(crate) fn normalize_git_diff_hunk_header(line: &str, new_file_block: bool) -> Option<String> {
    let ranges = line.strip_prefix("@@ ")?;
    let (ranges, context) = ranges.split_once(" @@")?;
    let (old, new) = ranges.split_once(' ')?;
    if !old.starts_with('-') || !new.starts_with('+') {
        return None;
    }

    fn omit_unit_count(range: &str, new_file_block: bool) -> Option<String> {
        let (sign, coordinates) = range.split_at(1);
        if !matches!(sign, "-" | "+") {
            return None;
        }
        let (start, count) = coordinates
            .split_once(',')
            .map_or((coordinates, None), |(start, count)| (start, Some(count)));
        if start.is_empty() || !start.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        match count {
            // Omitted count means exactly one line (POSIX unified diff).
            Some("1") => Some(format!("{sign}{start}")),
            // For a whole-new-file hunk the old range is empty at the very top; git
            // numbers that start as 0 and gix as 1. Canonicalize only that verified
            // context, and keep the explicit zero count.
            Some("0") if new_file_block && matches!(start, "0" | "1") => Some(format!("{sign}0,0")),
            // Zero-line counts stay explicit; all other counts pass through.
            Some(count) if !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit()) => {
                Some(format!("{sign}{start},{count}"))
            }
            None => Some(range.to_string()),
            _ => None,
        }
    }

    Some(format!(
        "@@ {} {} @@{}",
        omit_unit_count(old, new_file_block)?,
        omit_unit_count(new, new_file_block)?,
        context
    ))
}

pub(crate) fn named_array_items(items: &[Value]) -> Option<BTreeMap<&str, &Value>> {
    let mut keyed = BTreeMap::new();
    for item in items {
        let name = named_array_item_name(item)?;
        if keyed.insert(name, item).is_some() {
            return None;
        }
    }
    Some(keyed)
}

pub(crate) fn named_array_item_name(item: &Value) -> Option<&str> {
    item.as_object()?.get("name")?.as_str()
}
