//! 运行报告产出（summary/markdown/jsonl）、diff-rules 加载与应用。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use reqwest::header::HeaderMap;
use serde::Serialize;
use serde_json::{Value, json};

use crate::types::*;

pub(crate) fn write_summary(path: &Path, run_id: &str, diff: &RunDiff) -> Result<()> {
    let mut summary = fs::File::create(path)?;
    writeln!(summary, "# File-server A/B run {run_id}\n")?;
    writeln!(summary, "- Comparisons: {}", diff.summary.comparisons)?;
    writeln!(summary, "- Equal: {}", diff.summary.equal)?;
    writeln!(
        summary,
        "- Expected differences: {}",
        diff.summary.expected_differences
    )?;
    writeln!(
        summary,
        "- Unclassified differences: {}\n",
        diff.summary.unclassified_differences
    )?;
    writeln!(
        summary,
        "- Environment errors: {}",
        diff.summary.environment_errors
    )?;
    writeln!(
        summary,
        "- Environment blocked: {}\n",
        diff.summary.environment_blocked
    )?;
    writeln!(
        summary,
        "- Precondition errors: {}\n",
        diff.summary.precondition_errors
    )?;
    writeln!(
        summary,
        "- Transport failures (cause unclassified): {}\n",
        diff.summary.transport_errors
    )?;
    writeln!(summary, "- Blocked cases: {}", diff.summary.blocked_cases)?;
    if let Some(reason) = &diff.summary.incomplete_reason {
        writeln!(
            summary,
            "- **Incomplete run**: the driver exited before finishing; cases and requests below are partial. {reason}\n"
        )?;
    }
    writeln!(summary, "## HTTP cases\n")?;
    writeln!(
        summary,
        "| Case | Result | Rust HTTP | TS HTTP | Differences |\n|---|---:|---:|---:|---:|"
    )?;
    for case in &diff.cases {
        writeln!(
            summary,
            "| `{}` | {} | {} | {} | {} |",
            case.case,
            if case.blocked_by.is_some() {
                "blocked"
            } else if case.equal {
                "equal"
            } else {
                "different"
            },
            display_status(case.rust_status),
            display_status(case.ts_status),
            case.differences.len()
        )?;
    }
    writeln!(summary, "\n## Difference field summary (reporting only)\n")?;
    writeln!(
        summary,
        "Counts repeated fields by scope, JSON/header path, kind, and classification. This section does not normalize or waive any difference; `diff.json` remains the full source of truth.\n"
    )?;
    writeln!(
        summary,
        "| Scope | Path | Kind | Classification | Occurrences | Cases (up to 5 shown) |\n|---|---|---|---|---:|---|"
    )?;
    for group in difference_groups(diff) {
        let shown_cases = group.cases.iter().take(5).cloned().collect::<Vec<_>>();
        let omitted = group.cases.len().saturating_sub(shown_cases.len());
        let cases = if omitted == 0 {
            shown_cases.join(", ")
        } else {
            format!("{}, … (+{omitted} more)", shown_cases.join(", "))
        };
        writeln!(
            summary,
            "| {} | {} | {} | {} | {} | {} |",
            markdown_table_cell(&group.scope),
            markdown_table_cell(&group.path),
            markdown_table_cell(&group.kind),
            if group.accepted {
                "expected"
            } else {
                "unclassified"
            },
            group.occurrences,
            markdown_table_cell(&cases)
        )?;
    }
    if diff.cases.iter().all(|case| case.differences.is_empty())
        && diff.initial_state_differences.is_empty()
        && diff.state_differences.is_empty()
        && diff.git_state_differences.is_empty()
    {
        writeln!(summary, "| — | — | — | — | 0 | No differences |")?;
    }
    writeln!(summary, "\n## Initial workspace state differences\n")?;
    if diff.initial_state_differences.is_empty() {
        writeln!(summary, "No initial file-tree differences.")?;
    } else {
        for difference in &diff.initial_state_differences {
            writeln!(summary, "- `{}` `{}`", difference.path, difference.kind)?;
        }
    }
    writeln!(summary, "\n## Final workspace state differences\n")?;
    if diff.state_differences.is_empty() {
        writeln!(summary, "No file-tree differences.")?;
    } else {
        for difference in &diff.state_differences {
            writeln!(summary, "- `{}` `{}`", difference.path, difference.kind)?;
        }
    }
    writeln!(summary, "\n## Git ref, index and status differences\n")?;
    if diff.git_state_differences.is_empty() {
        writeln!(summary, "No captured Git-state differences.")?;
    } else {
        for difference in &diff.git_state_differences {
            writeln!(summary, "- `{}` `{}`", difference.path, difference.kind)?;
        }
    }
    writeln!(summary, "\n## Classified outcome\n")?;
    writeln!(
        summary,
        "Every difference is bucketed by cause; `diff.json` remains the source of truth. Strict exit rules are unchanged by this section.\n"
    )?;
    let mut buckets: BTreeMap<&str, (usize, BTreeSet<String>)> = BTreeMap::new();
    let mut bucket = |name: &'static str, case: &str| {
        let entry = buckets.entry(name).or_insert((0, BTreeSet::new()));
        entry.0 += 1;
        entry.1.insert(case.to_string());
    };
    let mut all_differences = diff
        .cases
        .iter()
        .flat_map(|case| {
            case.differences
                .iter()
                .map(move |d| (case.case.as_str(), d))
        })
        .collect::<Vec<_>>();
    all_differences.extend(
        diff.state_differences
            .iter()
            .map(|d| ("workspace-state", d))
            .chain(diff.git_state_differences.iter().map(|d| ("git-state", d)))
            .chain(
                diff.initial_state_differences
                    .iter()
                    .map(|d| ("initial-state", d)),
            ),
    );
    for (case_name, difference) in all_differences {
        if difference.accepted {
            bucket("approved differences (rules)", case_name);
        } else if difference.kind == "blocked" {
            bucket("blocked cases", case_name);
        } else if difference.kind == "transport_error" {
            bucket("transport failures (cause unclassified)", case_name);
        } else if difference.kind == "assertion_failed" && difference.path.contains("/typescript/")
        {
            bucket("TypeScript-side contract failures", case_name);
        } else if difference.kind == "assertion_failed" && difference.path.contains("/rust/") {
            bucket("Rust-side contract failures", case_name);
        } else if difference.kind == "assertion_failed" {
            bucket("unattributed assertion failures", case_name);
        } else {
            bucket("unexplained differences", case_name);
        }
    }
    for (name, (count, cases)) in &buckets {
        let shown = cases.iter().take(6).cloned().collect::<Vec<_>>();
        let omitted = cases.len().saturating_sub(shown.len());
        let case_list = if omitted == 0 {
            shown.join(", ")
        } else {
            format!("{shown:?} (+{omitted} more)")
        };
        writeln!(summary, "- **{name}**: {count} — cases: {case_list}")?;
    }

    writeln!(summary, "\n## Exact comparison normalizations\n")?;
    let mut any_normalizations = false;
    for case in &diff.cases {
        if !case.normalized_paths.is_empty() || !case.normalized_headers.is_empty() {
            any_normalizations = true;
            let mut normalizations = case
                .normalized_paths
                .iter()
                .map(|path| format!("JSON `{path}`"))
                .collect::<Vec<_>>();
            normalizations.extend(
                case.normalized_headers
                    .iter()
                    .map(|header| format!("header `{header}`")),
            );
            writeln!(summary, "- `{}`: {}", case.case, normalizations.join(", "))?;
        }
    }
    if !any_normalizations {
        writeln!(summary, "No JSON fields or response headers normalized.")?;
    }
    Ok(())
}

pub(crate) fn difference_groups(diff: &RunDiff) -> Vec<DifferenceGroup> {
    let mut grouped: BTreeMap<DifferenceGroupKey, DifferenceGroupCounts> = BTreeMap::new();
    let mut add = |scope: &str, case: &str, difference: &Difference| {
        let key = (
            scope.to_string(),
            difference.path.clone(),
            difference.kind.clone(),
            difference.accepted,
        );
        let (occurrences, cases) = grouped.entry(key).or_default();
        *occurrences += 1;
        cases.insert(case.to_string());
    };

    for case in &diff.cases {
        for difference in &case.differences {
            add("HTTP", &case.case, difference);
        }
    }
    for difference in &diff.initial_state_differences {
        add("initial workspace", "initial snapshot", difference);
    }
    for difference in &diff.state_differences {
        add("workspace", "final snapshot", difference);
    }
    for difference in &diff.git_state_differences {
        add("Git state", "Git snapshot", difference);
    }

    let mut groups = grouped
        .into_iter()
        .map(
            |((scope, path, kind, accepted), (occurrences, cases))| DifferenceGroup {
                scope,
                path,
                kind,
                accepted,
                occurrences,
                cases,
            },
        )
        .collect::<Vec<_>>();
    groups.sort_by(|left, right| {
        right
            .occurrences
            .cmp(&left.occurrences)
            .then_with(|| left.scope.cmp(&right.scope))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.accepted.cmp(&right.accepted))
    });
    groups
}

pub(crate) fn markdown_table_cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\n', '\r'], " ")
}

pub(crate) fn display_status(status: Option<u16>) -> String {
    status.map_or_else(|| "transport error".into(), |value| value.to_string())
}

pub(crate) fn diff(
    case: &str,
    path: &str,
    kind: &str,
    rust_value: Option<Value>,
    ts_value: Option<Value>,
) -> Difference {
    Difference {
        case: case.to_string(),
        path: if path.is_empty() {
            "/".into()
        } else {
            path.into()
        },
        kind: kind.into(),
        rust_value,
        ts_value,
        accepted: false,
        reason: None,
    }
}

pub(crate) fn append_jsonl(writer: &mut fs::File, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

pub(crate) fn headers_map(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value.to_str().ok().map(|value| {
                (
                    name.as_str().to_string(),
                    redact_header(name.as_str(), value),
                )
            })
        })
        .collect()
}

pub(crate) fn redact_header(name: &str, value: &str) -> String {
    if matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "cookie" | "set-cookie" | "x-proxy-token"
    ) {
        "[REDACTED]".into()
    } else {
        value.to_string()
    }
}

pub(crate) fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

pub(crate) fn validate_rules(rules: &RulesFile) -> Result<()> {
    if rules.schema_version != 1 {
        bail!(
            "unsupported A/B rules schema version {}",
            rules.schema_version
        );
    }
    let today = chrono::Utc::now().date_naive();
    let mut keys = BTreeSet::new();
    for rule in &rules.rules {
        if rule.case.trim().is_empty()
            || rule.path.trim().is_empty()
            || !rule.path.starts_with('/')
            || rule.path.contains('*')
            || rule.kind.trim().is_empty()
            || rule.reason.trim().is_empty()
            || rule.reviewed_by.trim().is_empty()
        {
            bail!("each A/B rule requires exact case/path/kind, a reason, and reviewed_by");
        }
        if !matches!(
            rule.kind.as_str(),
            "value" | "rust_only" | "ts_only" | "bytes"
        ) {
            bail!(
                "A/B rules may accept only exact semantic value differences, not {}",
                rule.kind
            );
        }
        let expires_on = chrono::NaiveDate::parse_from_str(&rule.expires_on, "%Y-%m-%d")
            .with_context(|| format!("invalid A/B rule expiry date {}", rule.expires_on))?;
        if expires_on < today {
            bail!(
                "A/B difference rule for {} {} expired on {}",
                rule.case,
                rule.path,
                rule.expires_on
            );
        }
        let key = (&rule.case, &rule.path, &rule.kind);
        if !keys.insert(key) {
            bail!(
                "duplicate A/B difference rule for {} {} {}",
                rule.case,
                rule.path,
                rule.kind
            );
        }
    }
    Ok(())
}

pub(crate) fn apply_rules(differences: &mut [Difference], rules: &RulesFile) {
    for difference in differences {
        let Some(rule) = rules.rules.iter().find(|rule| {
            rule.case == difference.case
                && rule.path == difference.path
                && rule.kind == difference.kind
                && expected_value_matches(&difference.rust_value, &rule.expected_rust)
                && expected_value_matches(&difference.ts_value, &rule.expected_typescript)
        }) else {
            continue;
        };
        difference.accepted = true;
        difference.reason = Some(format!(
            "{} (reviewed by {}, expires {})",
            rule.reason, rule.reviewed_by, rule.expires_on
        ));
    }
}

pub(crate) fn expected_value_matches(actual: &Option<Value>, expected: &Value) -> bool {
    if expected == &json!({"$missing": true}) {
        actual.is_none()
    } else {
        actual.as_ref() == Some(expected)
    }
}
