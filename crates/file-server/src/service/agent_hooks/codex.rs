//! Codex hooks 转换 (对齐 nuwax `transformHooksForCodex` + 配套 shell 脚本辅助)。
//!
//! 把 Claude 格式 hooks 转为 Codex `hooks.json`: 仅保留 Codex 官方生命周期事件,
//! command handler 经 `normalizeCodexCommand` 规范, http handler 生成 `http-hook-N.sh`
//! curl wrapper 脚本后转 command handler。

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};
use tokio::fs;

use crate::error::AppResult;

use super::io_util::write_file_atomic;

/// Codex 官方文档支持的生命周期事件 (CODEX_HOOK_EVENTS)。
const CODEX_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SubagentStart",
    "SubagentStop",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
    "UserPromptSubmit",
    "Stop",
];

// ── shell / 脚本路径辅助 (对齐 nuwax) ───────────────────────────────────────────

/// 解析 timeout 秒数 (对齐 nuwax parseTimeoutSeconds): 非有限正数 → 默认值。
fn parse_timeout_seconds(value: &Value, default_sec: u32) -> u32 {
    let n = match value {
        Value::Number(num) => num.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        Value::Bool(true) => Some(1.0),
        Value::Bool(false) => Some(0.0),
        _ => None,
    };
    match n {
        Some(x) if x.is_finite() && x > 0.0 => (x.min(86_400.0)) as u32,
        _ => default_sec,
    }
}

/// Shell 单引号转义 (对齐 nuwax shellSingleQuote)。
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 将 header 值中的 `$ENV_VAR` 转为 bash 运行时展开形式 `${ENV_VAR}` (对齐 nuwax toBashEnvExpandable)。
fn to_bash_env_expandable(value: &str) -> String {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\$([a-zA-Z_][a-zA-Z0-9_]*)").expect("env var regex"));
    RE.replace_all(value, |caps: &regex::Captures| format!("${{{}}}", &caps[1]))
        .to_string()
}

/// 判断 command 是否为需基于 git 根目录解析的脚本路径 (对齐 nuwax isLikelyScriptPath)。
fn is_likely_script_path(command: &str) -> bool {
    static GIT_ROOT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"git rev-parse --show-toplevel").expect("git root regex"));
    static SHELL_META: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[|;&`<>$()]").expect("shell meta regex"));
    static CMD_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(bash|sh|zsh|dash|python3?|node|npm|npx|curl|wget|echo|env|cd|export|test|\[)\s+")
            .expect("cmd prefix regex")
    });
    static SCRIPT_EXT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\.(sh|bash|py|js|mjs|cjs|ts|pl|rb)$").expect("script ext regex")
    });
    static BARE_SH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^[\w.-]+\.sh$").expect("bare sh regex"));

    let trimmed = command.trim();
    if trimmed.is_empty() {
        return false;
    }
    if GIT_ROOT.is_match(trimmed) {
        return false;
    }
    if SHELL_META.is_match(trimmed) {
        return false;
    }
    if CMD_PREFIX.is_match(trimmed) {
        return false;
    }
    if trimmed.starts_with('/') {
        return true;
    }
    if trimmed.contains('/') || SCRIPT_EXT.is_match(trimmed) {
        return true;
    }
    if BARE_SH.is_match(trimmed) {
        return true;
    }
    false
}

/// 字符串反斜杠/双引号转义 (对齐 nuwax 多处 `.replace(/\\/g, "\\\\").replace(/"/g, '\\"')`)。
fn escape_for_double_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 把相对脚本路径规范为基于 git 根目录的 bash 调用 (对齐 nuwax normalizeCodexCommand)。
fn normalize_codex_command(command: &str) -> String {
    let trimmed = command.trim();
    if !is_likely_script_path(trimmed) {
        return trimmed.to_string();
    }
    if let Some(rest) = trimmed.strip_prefix('/') {
        // 绝对脚本路径 → bash "escaped"
        return format!("bash \"{}\"", escape_for_double_quote(&format!("/{rest}")));
    }
    let relative = trimmed.strip_prefix("./").unwrap_or(trimmed);
    let codex_hooks_prefix = ".codex/hooks/";
    if let Some(script_name) = relative.strip_prefix(codex_hooks_prefix)
        && !script_name.is_empty()
        && !script_name.contains('/')
    {
        return build_codex_hook_command(script_name);
    }
    static BARE_SH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^[\w.-]+\.sh$").expect("bare sh regex 2"));
    if BARE_SH.is_match(relative) {
        return build_codex_hook_command(relative);
    }
    format!(
        "bash \"$(git rev-parse --show-toplevel 2>/dev/null || pwd)/{}\"",
        escape_for_double_quote(relative)
    )
}

/// Codex hook 脚本路径解析前缀 (对齐 nuwax buildCodexHookCommand)。
fn build_codex_hook_command(script_name: &str) -> String {
    format!(
        "bash \"$(git rev-parse --show-toplevel 2>/dev/null || pwd)/.codex/hooks/{script_name}\""
    )
}

/// 构建 curl header 参数数组 (对齐 nuwax buildCurlHeaderArgs)。
fn build_curl_header_args(headers: Option<&Value>) -> Vec<String> {
    let mut args = Vec::new();
    let entries: Vec<(String, Value)> = match headers {
        Some(Value::Object(m)) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        _ => {
            args.push("-H \"Content-Type: application/json\"".to_string());
            return args;
        }
    };
    let has_content_type = entries
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
    if !has_content_type {
        args.push("-H \"Content-Type: application/json\"".to_string());
    }
    for (key, value) in entries {
        if value.is_null() {
            continue;
        }
        let val_str = match &value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            other => other.to_string(),
        };
        let header_key = escape_for_double_quote(&key);
        let header_value = escape_for_double_quote(&to_bash_env_expandable(&val_str));
        args.push(format!("-H \"{header_key}: {header_value}\""));
    }
    args
}

/// 构建 http hook 的 curl wrapper 脚本内容 (对齐 nuwax buildHttpWrapperScript)。
fn build_http_wrapper_script(url: &str, timeout_sec: u32, headers: Option<&Value>) -> String {
    let header_args = build_curl_header_args(headers).join(" ");
    format!(
        "#!/usr/bin/env bash\nset -euo pipefail\ncurl -fsS -X POST {header_args} --data-binary @- --max-time {timeout_sec} {}\n",
        shell_single_quote(url)
    )
}

// ── transformHooksForCodex ──────────────────────────────────────────────────────

/// 把 Claude 格式 hooks 转为 Codex hooks map (对齐 nuwax transformHooksForCodex):
/// - 仅保留 CODEX_HOOK_EVENTS 事件
/// - command handler: normalizeCodexCommand
/// - http handler: 生成 `http-hook-N.sh` wrapper 脚本 (0o755), 转 command handler
/// - prompt/agent/未知: 跳过 (warn)
///
/// wrapper 脚本写入 `codex_hooks_dir`; 返回 Codex hooks map (空则 None)。
pub(super) async fn transform_hooks_for_codex(
    hooks_map: &Map<String, Value>,
    codex_hooks_dir: &Path,
) -> AppResult<Option<Map<String, Value>>> {
    let allowed: HashSet<&str> = CODEX_HOOK_EVENTS.iter().copied().collect();
    let mut codex_hooks: Map<String, Value> = Map::new();
    let mut wrapper_index = 0u32;

    for (event_name, matcher_groups) in hooks_map {
        if !allowed.contains(event_name.as_str()) {
            tracing::warn!(event = %event_name, "Skipping unsupported Codex hook event");
            continue;
        }
        let Some(groups_arr) = matcher_groups.as_array() else {
            continue;
        };
        let mut transformed_groups: Vec<Value> = Vec::new();

        for group in groups_arr {
            let Some(group_obj) = group.as_object() else {
                continue;
            };
            let raw_handlers = group_obj.get("hooks").and_then(|h| h.as_array());
            let mut transformed_handlers: Vec<Value> = Vec::new();

            if let Some(raw_arr) = raw_handlers {
                for handler in raw_arr {
                    let Some(handler_obj) = handler.as_object() else {
                        continue;
                    };
                    let htype = handler_obj
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    // command handler
                    if htype == "command"
                        && let Some(cmd) = handler_obj.get("command").and_then(|v| v.as_str())
                    {
                        if !cmd.trim().is_empty() {
                            let mut new_handler = handler_obj.clone();
                            new_handler
                                .insert("type".to_string(), Value::String("command".to_string()));
                            new_handler.insert(
                                "command".to_string(),
                                Value::String(normalize_codex_command(cmd)),
                            );
                            transformed_handlers.push(Value::Object(new_handler));
                        }
                        continue;
                    }

                    // http handler → curl wrapper 脚本 + command handler
                    if htype == "http"
                        && let Some(url) = handler_obj.get("url").and_then(|v| v.as_str())
                        && !url.trim().is_empty()
                    {
                        let timeout = handler_obj
                            .get("timeout")
                            .map(|v| parse_timeout_seconds(v, 30))
                            .unwrap_or(30);
                        let script_name = format!("http-hook-{wrapper_index}.sh");
                        wrapper_index += 1;
                        let script_path = codex_hooks_dir.join(&script_name);
                        let headers = handler_obj.get("headers").filter(|v| v.is_object());
                        fs::create_dir_all(codex_hooks_dir).await?;
                        let script = build_http_wrapper_script(url.trim(), timeout, headers);
                        write_file_atomic(&script_path, &script, Some(0o755)).await?;

                        let mut new_handler = Map::new();
                        new_handler
                            .insert("type".to_string(), Value::String("command".to_string()));
                        new_handler.insert(
                            "command".to_string(),
                            Value::String(build_codex_hook_command(&script_name)),
                        );
                        new_handler.insert(
                            "timeout".to_string(),
                            Value::Number(serde_json::Number::from(timeout)),
                        );
                        if let Some(sm) = handler_obj.get("statusMessage").filter(|v| !v.is_null())
                        {
                            new_handler.insert("statusMessage".to_string(), sm.clone());
                        }
                        transformed_handlers.push(Value::Object(new_handler));
                        continue;
                    }

                    if htype == "prompt" || htype == "agent" {
                        tracing::warn!(
                            event = %event_name, handler_type = %htype,
                            "Codex skips non-command hook handler"
                        );
                        continue;
                    }
                    tracing::warn!(
                        event = %event_name, handler_type = %htype,
                        "Codex skips unknown hook handler"
                    );
                }
            }

            if !transformed_handlers.is_empty() {
                let mut new_group = group_obj.clone();
                new_group.insert("hooks".to_string(), Value::Array(transformed_handlers));
                transformed_groups.push(Value::Object(new_group));
            }
        }

        if !transformed_groups.is_empty() {
            codex_hooks.insert(event_name.clone(), Value::Array(transformed_groups));
        }
    }

    if codex_hooks.is_empty() {
        Ok(None)
    } else {
        Ok(Some(codex_hooks))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn timeout_parsing_defaults_on_invalid() {
        assert_eq!(parse_timeout_seconds(&Value::Null, 30), 30);
        assert_eq!(parse_timeout_seconds(&json!("abc"), 30), 30);
        assert_eq!(parse_timeout_seconds(&json!(0), 30), 30);
        assert_eq!(parse_timeout_seconds(&json!(-5), 30), 30);
        assert_eq!(parse_timeout_seconds(&json!(10), 30), 10);
        assert_eq!(parse_timeout_seconds(&json!("15"), 30), 15);
    }

    #[test]
    fn shell_single_quote_escapes() {
        assert_eq!(shell_single_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_single_quote("plain"), "'plain'");
    }

    #[test]
    fn bash_env_expand_rewrites() {
        assert_eq!(to_bash_env_expandable("Bearer $TOKEN"), "Bearer ${TOKEN}");
        assert_eq!(to_bash_env_expandable("$A/$B1"), "${A}/${B1}");
        assert_eq!(to_bash_env_expandable("no vars"), "no vars");
    }

    #[test]
    fn is_likely_script_path_classification() {
        assert!(is_likely_script_path("./run.sh"));
        assert!(is_likely_script_path("hooks/foo.sh"));
        assert!(is_likely_script_path("/abs/script.sh"));
        assert!(is_likely_script_path("tool.py"));
        // shell 元字符 / 命令前缀 → 不是脚本路径
        assert!(!is_likely_script_path("echo hi && cat x"));
        assert!(!is_likely_script_path("node index.js"));
        assert!(!is_likely_script_path("bash run.sh"));
        assert!(!is_likely_script_path("curl http://x"));
        assert!(!is_likely_script_path(""));
    }

    #[test]
    fn normalize_codex_command_relative_script() {
        // 相对脚本 → 包 git toplevel 前缀
        let cmd = normalize_codex_command("scripts/hook.sh");
        assert!(cmd.starts_with("bash \"$(git rev-parse --show-toplevel"));
        assert!(cmd.contains("scripts/hook.sh"));
    }

    #[test]
    fn normalize_codex_command_codex_hooks_short() {
        // .codex/hooks/<name>.sh (无子目录) → buildCodexHookCommand
        let cmd = normalize_codex_command(".codex/hooks/pre.sh");
        assert!(cmd.contains(".codex/hooks/pre.sh"));
        assert!(cmd.starts_with("bash \"$(git rev-parse"));
    }

    #[test]
    fn normalize_codex_command_passes_through_non_script() {
        assert_eq!(normalize_codex_command("node index.js"), "node index.js");
        assert_eq!(normalize_codex_command("echo hi"), "echo hi");
    }

    #[test]
    fn build_curl_header_args_default_content_type() {
        let args = build_curl_header_args(None);
        assert_eq!(args, vec!["-H \"Content-Type: application/json\""]);
        // 有 Content-Type → 不再补默认
        let h = json!({"Content-Type": "text/plain", "X-Api-Key": "k$SECRET"});
        let args = build_curl_header_args(Some(&h));
        assert!(args.iter().any(|a| a.contains("text/plain")));
        assert!(args.iter().any(|a| a.contains("X-Api-Key")));
        // $SECRET → ${SECRET}
        assert!(args.iter().any(|a| a.contains("${SECRET}")));
        assert!(!args.iter().any(|a| a.contains("application/json")));
    }

    #[test]
    fn build_http_wrapper_script_shape() {
        let script = build_http_wrapper_script("https://x.com/h", 30, None);
        assert!(script.starts_with("#!/usr/bin/env bash\nset -euo pipefail"));
        assert!(script.contains("curl -fsS -X POST"));
        assert!(script.contains("--max-time 30"));
        assert!(script.contains("'https://x.com/h'"));
    }
}
