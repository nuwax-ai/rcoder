//! pnpm NDJSON wire protocol：事件聚合与用户日志渲染。

use serde_json::{Map, Value};

use super::types::InstallSummary;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ObservedLine {
    Unstructured,
    Suppressed,
    Rendered(String),
}

pub(super) fn observe_event(line: &str, summary: &mut InstallSummary) -> ObservedLine {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return ObservedLine::Unstructured;
    };
    let Some(object) = value.as_object() else {
        return ObservedLine::Unstructured;
    };
    let name = string_at(object, &["name"]).unwrap_or("pnpm");
    summary.event_count += 1;
    *summary.events_by_name.entry(name.to_string()).or_default() += 1;

    let level = string_at(object, &["level"]);
    if level == Some("warn") {
        summary.warning_count += 1;
    }
    if name == "pnpm:stats" {
        summary.added = unsigned_at(object, &["added"]).or(summary.added);
        summary.removed = unsigned_at(object, &["removed"]).or(summary.removed);
    } else if name == "pnpm:context" {
        summary.store_dir = string_at(object, &["storeDir"]).map(ToOwned::to_owned);
    }

    if let Some(code) = event_code(object)
        && !summary.error_codes.iter().any(|current| current == code)
    {
        summary.error_codes.push(code.to_string());
    }
    let message = event_message(object);
    if (level == Some("error") || event_code(object).is_some())
        && let Some(message) = message
    {
        summary.push_diagnostic(message);
    }

    render_event(name, object, level, message)
}

fn render_event(
    name: &str,
    object: &Map<String, Value>,
    level: Option<&str>,
    message: Option<&str>,
) -> ObservedLine {
    if let Some(message) = message {
        return ObservedLine::Rendered(format!("[{name}] {message}"));
    }
    if let Some(line) = string_at(object, &["line"]) {
        return ObservedLine::Rendered(format!("[{name}] {line}"));
    }
    if name == "pnpm:stats" {
        return ObservedLine::Rendered(format!(
            "[{name}] added={}, removed={}",
            unsigned_at(object, &["added"]).unwrap_or(0),
            unsigned_at(object, &["removed"]).unwrap_or(0)
        ));
    }
    if name == "pnpm:context" {
        let store = string_at(object, &["storeDir"]).unwrap_or("unknown");
        return ObservedLine::Rendered(format!("[{name}] store={store}"));
    }
    if name == "pnpm:lifecycle" {
        let stage = string_at(object, &["stage"]).unwrap_or("script");
        let package = string_at(object, &["depPath"]).unwrap_or("dependency");
        if let Some(script) = string_at(object, &["script"]) {
            return ObservedLine::Rendered(format!("[{name}] {package} {stage}: {script}"));
        }
        if let Some(exit_code) = signed_at(object, &["exitCode"]) {
            return ObservedLine::Rendered(format!(
                "[{name}] {package} {stage} exited with {exit_code}"
            ));
        }
    }
    if name == "pnpm:ignored-scripts"
        && let Some(packages) = object.get("packageNames").and_then(Value::as_array)
        && !packages.is_empty()
    {
        let packages = packages
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        return ObservedLine::Rendered(format!("[{name}] blocked: {packages}"));
    }
    if name == "pnpm:summary" {
        return ObservedLine::Rendered(format!("[{name}] install complete"));
    }
    if name == "pnpm:execution-time"
        && let (Some(started), Some(ended)) = (
            unsigned_at(object, &["startedAt"]),
            unsigned_at(object, &["endedAt"]),
        )
    {
        return ObservedLine::Rendered(format!("[{name}] {} ms", ended.saturating_sub(started)));
    }
    if let Some(stage) = string_at(object, &["stage"]) {
        return ObservedLine::Rendered(format!("[{name}] {stage}"));
    }
    if matches!(level, Some("warn" | "error")) {
        let code = event_code(object).unwrap_or("pnpm");
        return ObservedLine::Rendered(format!("[{name}] {code}"));
    }
    // resolution/progress/package-manifest 等高频 debug 事件只进入摘要计数。
    ObservedLine::Suppressed
}

fn event_code(object: &Map<String, Value>) -> Option<&str> {
    string_at(object, &["code"])
        .or_else(|| nested_string(object, "err", "code"))
        .or_else(|| nested_string(object, "error", "code"))
}

fn event_message(object: &Map<String, Value>) -> Option<&str> {
    string_at(object, &["message"])
        .or_else(|| string_at(object, &["msg"]))
        .or_else(|| nested_string(object, "err", "message"))
        .or_else(|| nested_string(object, "error", "message"))
        .or_else(|| string_at(object, &["deprecated"]))
        .or_else(|| string_at(object, &["hint"]))
}

fn string_at<'a>(object: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
}

fn unsigned_at(object: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_u64))
}

fn signed_at(object: &Map<String, Value>, keys: &[&str]) -> Option<i64> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_i64))
}

fn nested_string<'a>(object: &'a Map<String, Value>, parent: &str, key: &str) -> Option<&'a str> {
    object.get(parent)?.as_object()?.get(key)?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observes_stats_context_and_nested_error() {
        let mut summary = InstallSummary::default();
        drop(observe_event(
            r#"{"name":"pnpm:context","level":"debug","storeDir":"/cache/store"}"#,
            &mut summary,
        ));
        drop(observe_event(
            r#"{"name":"pnpm:stats","level":"info","added":12,"removed":3}"#,
            &mut summary,
        ));
        drop(observe_event(
            r#"{"name":"pnpm","level":"error","err":{"code":"ERR_PNPM_FETCH_401","message":"Unauthorized"}}"#,
            &mut summary,
        ));

        assert_eq!(summary.event_count, 3);
        assert_eq!(summary.store_dir.as_deref(), Some("/cache/store"));
        assert_eq!(summary.added, Some(12));
        assert_eq!(summary.removed, Some(3));
        assert_eq!(summary.error_codes, ["ERR_PNPM_FETCH_401"]);
        assert_eq!(summary.diagnostics, ["Unauthorized"]);
    }

    #[test]
    fn non_json_output_is_not_counted_as_protocol_event() {
        let mut summary = InstallSummary::default();
        assert_eq!(
            observe_event("plain stderr", &mut summary),
            ObservedLine::Unstructured
        );
        assert_eq!(summary.event_count, 0);
    }

    #[test]
    fn suppresses_high_volume_progress_but_renders_stages_and_messages() {
        let mut summary = InstallSummary::default();
        assert_eq!(
            observe_event(
                r#"{"name":"pnpm:progress","level":"debug","status":"resolved","packageId":"react@18.3.1"}"#,
                &mut summary,
            ),
            ObservedLine::Suppressed
        );
        assert_eq!(
            observe_event(
                r#"{"name":"pnpm:stage","level":"debug","stage":"resolution_started"}"#,
                &mut summary,
            ),
            ObservedLine::Rendered("[pnpm:stage] resolution_started".to_string())
        );
        assert_eq!(
            observe_event(
                r#"{"name":"pnpm","level":"debug","msg":"loading workspace state"}"#,
                &mut summary,
            ),
            ObservedLine::Rendered("[pnpm] loading workspace state".to_string())
        );
        assert_eq!(
            observe_event(
                r#"{"name":"pnpm:lifecycle","level":"debug","depPath":"vue-demi@0.14.10","stage":"postinstall","exitCode":0}"#,
                &mut summary,
            ),
            ObservedLine::Rendered(
                "[pnpm:lifecycle] vue-demi@0.14.10 postinstall exited with 0".to_string()
            )
        );
    }
}
