//! Agent hook 配置写入 (对齐 nuwax `utils/computer/hookConfigUtils.js`)。
//!
//! 写入 Claude Code / Codex / OpenCode 三套 Hook 相关配置:
//! - Claude Code: `.claude/settings.json` (hooks + permissions) + `.mcp.json` + `.claude/hooks/` 脚本
//! - Codex: `.codex/hooks.json` (http hook 转 command wrapper 脚本) + `.codex/hooks/*.sh`
//! - OpenCode: `.opencode/plugins/opencode-hooks-plugin` + 可选 platform-env 插件
//!
//! 设计要点 (与 nuwax 一致):
//! - 仅在对应配置解析成功时才清除并重写, 避免无效 payload 误删旧配置
//! - Codex/OpenCode 运行时产物先在 `.tmp/hook-staging-*` staging 目录预生成, 成功后再替换工作区,
//!   缩小半更新窗口
//! - 原子写 (staging tmp + rename) + 损坏 JSON 保护 (解析失败保留旧文件)
//! - hook 脚本路径校验防穿越 (限定在 `.claude/` 下)
//!
//! hook 配置是 schemaless 的嵌套 JSON, 本模块用 `serde_json::Value` 透传处理。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};
use tokio::fs;

use crate::error::{AppError, AppResult};

// ── 常量 (对齐 nuwax) ───────────────────────────────────────────────────────────

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

const OPENCODE_PLUGIN_ENTRY: &str = "opencode-hooks-plugin.js";
const OPENCODE_PLUGIN_DIR: &str = "opencode-hooks-plugin";
const OPENCODE_PLATFORM_ENV_PLUGIN_ENTRY: &str = "opencode-platform-env-plugin.js";
const PLATFORM_ENV_SCRIPT_PATH: &str = "hooks/platform-env.sh";

// ── vendored opencode 插件 (编译进二进制) ────────────────────────────────────────

/// opencode-hooks-plugin/dist 下所有 .js (对齐 nuwax assets/opencode-hooks-plugin/dist)。
const OPENCODE_HOOKS_PLUGIN_FILES: &[(&str, &[u8])] = &[
    (
        "config.js",
        include_bytes!("../../assets/opencode-hooks-plugin/dist/config.js"),
    ),
    (
        "events.js",
        include_bytes!("../../assets/opencode-hooks-plugin/dist/events.js"),
    ),
    (
        "executor.js",
        include_bytes!("../../assets/opencode-hooks-plugin/dist/executor.js"),
    ),
    (
        "index.js",
        include_bytes!("../../assets/opencode-hooks-plugin/dist/index.js"),
    ),
    (
        "matcher.js",
        include_bytes!("../../assets/opencode-hooks-plugin/dist/matcher.js"),
    ),
    (
        "types.js",
        include_bytes!("../../assets/opencode-hooks-plugin/dist/types.js"),
    ),
];

/// opencode-platform-env-plugin 入口 (对齐 nuwax assets/opencode-platform-env-plugin)。
const OPENCODE_PLATFORM_ENV_PLUGIN_JS: &[u8] =
    include_bytes!("../../assets/opencode-platform-env-plugin/platform-env-plugin.js");

// ── 公共输入类型 ─────────────────────────────────────────────────────────────────

/// 单个 hook 外挂脚本 (对齐 nuwax hookScripts 数组项: {path, content})。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct HookScript {
    pub path: String,
    pub content: String,
}

/// write_agent_hook_configs 输入 (对齐 nuwax writeAgentHookConfigs 的 options)。
#[derive(Debug, Default)]
pub struct HookConfigInput {
    /// mcpServersConfig: JSON 字符串, 解析后写入 `.mcp.json` 的 mcpServers 字段。
    pub mcp_servers_config: Option<String>,
    /// hooksConfig: JSON 字符串, 解析后写入 `.claude/settings.json` hooks + 转 Codex hooks.json。
    pub hooks_config: Option<String>,
    /// permissionsConfig: JSON 字符串, 解析后写入 `.claude/settings.json` permissions 字段。
    pub permissions_config: Option<String>,
    /// hookScripts: 外挂脚本数组, 写入 `.claude/hooks/`。
    pub hook_scripts: Option<Vec<HookScript>>,
}

// ── hooks 配置解析 (parseHooksConfigWithStatus / normalizeHooksMap / parseHookHandlerObject) ──

/// hooks 配置解析状态 (对齐 nuwax { attempted, hooksMap, error })。
struct HooksStatus {
    attempted: bool,
    hooks_map: Option<Map<String, Value>>,
    error: Option<String>,
}

/// 解析 hooksConfig 字符串为规范化的 hooks map (对齐 nuwax parseHooksConfigWithStatus)。
/// 空输入 → 未尝试 (attempted=false); 解析失败 → attempted=true + error; 成功 → 规范化 hooksMap (可能为 None)。
fn parse_hooks_config_with_status(hooks_config: Option<&str>) -> HooksStatus {
    let Some(s) = hooks_config else {
        return HooksStatus {
            attempted: false,
            hooks_map: None,
            error: None,
        };
    };
    if s.trim().is_empty() {
        return HooksStatus {
            attempted: false,
            hooks_map: None,
            error: None,
        };
    }
    match serde_json::from_str::<Value>(s) {
        Ok(parsed) => HooksStatus {
            attempted: true,
            hooks_map: normalize_hooks_map(&parsed),
            error: None,
        },
        Err(e) => HooksStatus {
            attempted: true,
            hooks_map: None,
            error: Some(e.to_string()),
        },
    }
}

/// 规范化 hooks 配置: 确保 hooks 数组内为对象而非 JSON 字符串 (对齐 nuwax normalizeHooksMap)。
/// 非 object 输入 → None; 否则逐事件/分组/handler 规范化, 空结果 → None。
fn normalize_hooks_map(hooks_map: &Value) -> Option<Map<String, Value>> {
    let obj = hooks_map.as_object()?;
    let mut normalized = Map::new();
    for (event_name, matcher_groups) in obj {
        let Some(groups_arr) = matcher_groups.as_array() else {
            continue;
        };
        let mut groups: Vec<Value> = Vec::new();
        for group in groups_arr {
            let Some(group_obj) = group.as_object() else {
                continue;
            };
            let raw_handlers = group_obj.get("hooks").and_then(|h| h.as_array());
            let mut handlers: Vec<Value> = Vec::new();
            if let Some(raw_arr) = raw_handlers {
                for raw_handler in raw_arr {
                    if let Some(handler) = parse_hook_handler_object(raw_handler) {
                        handlers.push(handler);
                    }
                }
            }
            if handlers.is_empty() {
                continue;
            }
            let mut new_group = group_obj.clone();
            new_group.insert("hooks".to_string(), Value::Array(handlers));
            groups.push(Value::Object(new_group));
        }
        if !groups.is_empty() {
            normalized.insert(event_name.clone(), Value::Array(groups));
        }
    }
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

/// 把单个 handler 规范化为 object (对齐 nuwax parseHookHandlerObject):
/// - 已是 object → 原样返回
/// - 是 JSON 字符串 → 解析, 解析为 object 则返回, 否则 None
/// - null / 数字 / 布尔 / 数组 → None
fn parse_hook_handler_object(value: &Value) -> Option<Value> {
    match value {
        Value::Object(_) => Some(value.clone()),
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return None;
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(parsed) if parsed.is_object() => Some(parsed),
                _ => None,
            }
        }
        _ => None,
    }
}

// ── shell / 脚本路径辅助 ─────────────────────────────────────────────────────────

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

// ── 原子文件写入 (writeFileAtomic / writeJsonFileAtomic) ──────────────────────────

/// 当前时间纳秒 (用于生成唯一临时名)。
fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// 原子写文本文件: 先写 `.<name>.<pid>.<nanos>.tmp` 再 rename (对齐 nuwax writeFileAtomic)。
async fn write_file_atomic(target: &Path, content: &str, mode: Option<u32>) -> AppResult<()> {
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).await?;
    let basename = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let tmp = dir.join(format!(
        ".{basename}.{}.{}.tmp",
        std::process::id(),
        now_nanos()
    ));
    fs::write(&tmp, content).await?;
    if let Some(m) = mode {
        set_mode(&tmp, m).await?;
    }
    fs::rename(&tmp, target).await?;
    Ok(())
}

/// 原子写 JSON 文件 (对齐 nuwax writeJsonFileAtomic: pretty + 末尾换行)。
async fn write_json_file_atomic(target: &Path, data: &Value) -> AppResult<()> {
    let mut s = serde_json::to_string_pretty(data)
        .map_err(|e| AppError::system(format!("serialize json: {e}")))?;
    s.push('\n');
    write_file_atomic(target, &s, None).await
}

/// 设置文件权限 (unix only; 0o755 等)。
#[cfg(unix)]
async fn set_mode(path: &Path, mode: u32) -> AppResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
    Ok(())
}
#[cfg(not(unix))]
async fn set_mode(_path: &Path, _mode: u32) -> AppResult<()> {
    Ok(())
}

// ── Codex hooks 转换 (transformHooksForCodex) ────────────────────────────────────

/// 把 Claude 格式 hooks 转为 Codex hooks map (对齐 nuwax transformHooksForCodex):
/// - 仅保留 CODEX_HOOK_EVENTS 事件
/// - command handler: normalizeCodexCommand
/// - http handler: 生成 `http-hook-N.sh` wrapper 脚本 (0o755), 转 command handler
/// - prompt/agent/未知: 跳过 (warn)
///
/// wrapper 脚本写入 `codex_hooks_dir`; 返回 Codex hooks map (空则 None)。
async fn transform_hooks_for_codex(
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

// ── OpenCode 插件安装 (installOpencodeHooksPlugin / installOpencodePlatformEnvPlugin) ──

/// 安装 vendored opencode-hooks-plugin 到目标 plugins 目录 (对齐 nuwax installOpencodeHooksPlugin)。
/// 写 dist/*.js + 入口 re-export 文件。
async fn install_opencode_hooks_plugin(opencode_plugins_dir: &Path) -> AppResult<bool> {
    fs::create_dir_all(opencode_plugins_dir).await?;
    let target_plugin_root = opencode_plugins_dir.join(OPENCODE_PLUGIN_DIR).join("dist");
    fs::create_dir_all(&target_plugin_root).await?;
    for (name, content) in OPENCODE_HOOKS_PLUGIN_FILES {
        fs::write(target_plugin_root.join(name), content).await?;
    }
    let entry_file = opencode_plugins_dir.join(OPENCODE_PLUGIN_ENTRY);
    let entry_content =
        format!("export {{ default }} from \"./{OPENCODE_PLUGIN_DIR}/dist/index.js\";\n");
    write_file_atomic(&entry_file, &entry_content, None).await?;
    tracing::info!(
        entry = OPENCODE_PLUGIN_ENTRY,
        "Installed opencode-hooks-plugin into .opencode/plugins"
    );
    Ok(true)
}

/// 安装 vendored opencode-platform-env-plugin (对齐 nuwax installOpencodePlatformEnvPlugin)。
async fn install_opencode_platform_env_plugin(opencode_plugins_dir: &Path) -> AppResult<bool> {
    fs::create_dir_all(opencode_plugins_dir).await?;
    let target = opencode_plugins_dir.join(OPENCODE_PLATFORM_ENV_PLUGIN_ENTRY);
    fs::write(&target, OPENCODE_PLATFORM_ENV_PLUGIN_JS).await?;
    tracing::info!(
        entry = OPENCODE_PLATFORM_ENV_PLUGIN_ENTRY,
        "Installed opencode-platform-env-plugin into .opencode/plugins"
    );
    Ok(true)
}

/// 判断 hookScripts 是否含 platform-env 脚本 (对齐 nuwax hasPlatformEnvScript)。
fn has_platform_env_script(hook_scripts: Option<&[HookScript]>) -> bool {
    let Some(scripts) = hook_scripts else {
        return false;
    };
    scripts.iter().any(|s| s.path == PLATFORM_ENV_SCRIPT_PATH)
}

// ── .claude/settings.json 读取 (readClaudeSettings) ──────────────────────────────

/// 读取 .claude/settings.json (对齐 nuwax readClaudeSettings):
/// 不存在 → 空 object + 非 corrupt; 解析失败/非 object → 空 object + corrupt。
async fn read_claude_settings(settings_path: &Path) -> (Value, bool) {
    if !fs::try_exists(settings_path).await.unwrap_or(false) {
        return (Value::Object(Map::new()), false);
    }
    match fs::read_to_string(settings_path).await {
        Ok(content) => match serde_json::from_str::<Value>(&content) {
            Ok(v) if v.is_object() => (v, false),
            _ => (Value::Object(Map::new()), true),
        },
        Err(_) => (Value::Object(Map::new()), true),
    }
}

// ── hook 外挂脚本写入 (writeHookScripts) ─────────────────────────────────────────

/// 写入 hook 外挂脚本 (相对 .claude 目录; 路径校验防穿越, 0o755)。
/// 对齐 nuwax writeHookScripts: path.normalize 后 starts_with("..") 或 isAbsolute → 跳过。
async fn write_hook_scripts(claude_dir: &Path, hook_scripts: &[HookScript]) -> AppResult<()> {
    if hook_scripts.is_empty() {
        return Ok(());
    }
    let hooks_dir = claude_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).await?;
    for script in hook_scripts {
        if script.path.trim().is_empty() {
            continue;
        }
        // 路径校验: ensure_within 拦截 `..` 穿越与绝对路径 (等价 nuwax normalize + 前缀判断)
        let target = match crate::path_safety::ensure_within(claude_dir, script.path.trim()) {
            Ok(p) => p,
            Err(_) => {
                tracing::warn!(path = %script.path, "Hook script path contains traversal, skipping");
                continue;
            }
        };
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).await?;
        }
        write_file_atomic(&target, &script.content, Some(0o755)).await?;
        tracing::info!(path = %script.path, "Written hook script");
    }
    Ok(())
}

// ── 清理 (clearHookArtifacts) ────────────────────────────────────────────────────

/// 清除 hook 相关产物 (不含 permissions 等其他 settings 字段), 对齐 nuwax clearHookArtifacts。
async fn clear_hook_artifacts(workspace: &Path) -> AppResult<()> {
    let targets: [PathBuf; 6] = [
        workspace.join(".codex").join("hooks.json"),
        workspace.join(".codex").join("hooks"),
        workspace
            .join(".opencode")
            .join("plugins")
            .join(OPENCODE_PLUGIN_ENTRY),
        workspace
            .join(".opencode")
            .join("plugins")
            .join(OPENCODE_PLUGIN_DIR),
        workspace
            .join(".opencode")
            .join("plugins")
            .join(OPENCODE_PLATFORM_ENV_PLUGIN_ENTRY),
        workspace.join(".claude").join("hooks"),
    ];
    for target in targets {
        remove_path_best_effort(&target).await;
    }
    Ok(())
}

/// 删除文件或目录, NotFound 不计错 (对齐 nuwax fs.rm { recursive, force })。
async fn remove_path_best_effort(path: &Path) {
    let meta = match fs::symlink_metadata(path).await {
        Ok(m) => m,
        Err(_) => return,
    };
    let r = if meta.is_dir() {
        fs::remove_dir_all(path).await
    } else {
        fs::remove_file(path).await
    };
    if let Err(e) = r
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), error = %e, "remove hook artifact failed");
    }
}

// ── staging 预生成 + 应用 (stageRuntimeHookArtifacts / applyStagedRuntimeHookArtifacts) ──

/// staging 预生成产物 (对齐 nuwax stageRuntimeHookArtifacts)。
struct StagedRuntime {
    staging_root: PathBuf,
    codex_staging_root: PathBuf,
    codex_hooks: Option<Map<String, Value>>,
    plugin_installed: bool,
    platform_env_plugin_installed: bool,
}

/// 在 `.tmp/hook-staging-*` 预生成 Codex hooks + OpenCode 插件, 成功后再应用到工作区。
async fn stage_runtime_hook_artifacts(
    workspace: &Path,
    hooks_map: &Map<String, Value>,
    install_platform_env: bool,
) -> AppResult<StagedRuntime> {
    let staging_root = workspace.join(".tmp").join(format!(
        "hook-staging-{}-{}",
        now_nanos(),
        std::process::id()
    ));
    let codex_staging_root = staging_root.join("codex");
    let codex_hooks_dir = codex_staging_root.join("hooks");
    let opencode_plugins_staging = staging_root.join("opencode").join("plugins");

    fs::create_dir_all(&codex_hooks_dir).await?;
    let codex_hooks = transform_hooks_for_codex(hooks_map, &codex_hooks_dir).await?;
    let plugin_installed = install_opencode_hooks_plugin(&opencode_plugins_staging).await?;
    let platform_env_plugin_installed = if install_platform_env {
        install_opencode_platform_env_plugin(&opencode_plugins_staging).await?
    } else {
        false
    };

    Ok(StagedRuntime {
        staging_root,
        codex_staging_root,
        codex_hooks,
        plugin_installed,
        platform_env_plugin_installed,
    })
}

/// 将 staging 产物应用到工作区 (对齐 nuwax applyStagedRuntimeHookArtifacts)。
async fn apply_staged_runtime_hook_artifacts(
    workspace: &Path,
    staged: &StagedRuntime,
) -> AppResult<()> {
    let codex_root = workspace.join(".codex");
    let codex_hooks_target = codex_root.join("hooks");
    let opencode_plugins_target = workspace.join(".opencode").join("plugins");

    clear_hook_artifacts(workspace).await?;
    fs::create_dir_all(&codex_root).await?;

    // Codex hooks 目录 (含 http wrapper 脚本)
    let staged_codex_hooks_dir = staged.codex_staging_root.join("hooks");
    if fs::try_exists(&staged_codex_hooks_dir)
        .await
        .unwrap_or(false)
    {
        let _ = fs::remove_dir_all(&codex_hooks_target).await;
        fs::rename(&staged_codex_hooks_dir, &codex_hooks_target).await?;
    }

    // .codex/hooks.json
    if let Some(codex_hooks) = &staged.codex_hooks {
        write_json_file_atomic(
            &codex_root.join("hooks.json"),
            &json!({ "hooks": codex_hooks }),
        )
        .await?;
        tracing::info!(
            events = ?codex_hooks.keys().collect::<Vec<_>>(),
            "Written .codex/hooks.json"
        );
    }

    // opencode-hooks-plugin
    if staged.plugin_installed {
        fs::create_dir_all(&opencode_plugins_target).await?;
        let staged_plugins = staged.staging_root.join("opencode").join("plugins");
        let staged_entry = staged_plugins.join(OPENCODE_PLUGIN_ENTRY);
        if fs::try_exists(&staged_entry).await.unwrap_or(false) {
            fs::copy(
                &staged_entry,
                opencode_plugins_target.join(OPENCODE_PLUGIN_ENTRY),
            )
            .await?;
        }
        let staged_plugin_dir = staged_plugins.join(OPENCODE_PLUGIN_DIR);
        if fs::try_exists(&staged_plugin_dir).await.unwrap_or(false) {
            let target_dir = opencode_plugins_target.join(OPENCODE_PLUGIN_DIR);
            let _ = fs::remove_dir_all(&target_dir).await;
            crate::service::fs_util::copy_dir_filtered(&staged_plugin_dir, &target_dir, &[], &[])
                .await?;
        }
    }

    // opencode-platform-env-plugin
    if staged.platform_env_plugin_installed {
        fs::create_dir_all(&opencode_plugins_target).await?;
        let staged_pe = staged
            .staging_root
            .join("opencode")
            .join("plugins")
            .join(OPENCODE_PLATFORM_ENV_PLUGIN_ENTRY);
        if fs::try_exists(&staged_pe).await.unwrap_or(false) {
            fs::copy(
                &staged_pe,
                opencode_plugins_target.join(OPENCODE_PLATFORM_ENV_PLUGIN_ENTRY),
            )
            .await?;
        }
    }
    Ok(())
}

// ── 主入口 (writeAgentHookConfigs) ───────────────────────────────────────────────

/// 写入 Claude Code / Codex / OpenCode 三套 Hook 相关配置 (对齐 nuwax writeAgentHookConfigs)。
///
/// 仅在对应配置解析成功时才清除并重写; 任何 FS 错误按 nuwax 语义 best-effort 记录, 不向上抛
/// (避免破坏 workspace 创建主流程)。staging 目录在所有路径下都会被清理。
pub async fn write_agent_hook_configs(workspace: &Path, opts: HookConfigInput) -> AppResult<()> {
    let HookConfigInput {
        mcp_servers_config,
        hooks_config,
        permissions_config,
        hook_scripts,
    } = opts;

    let has_mcp_input = mcp_servers_config
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let hooks_status = parse_hooks_config_with_status(hooks_config.as_deref());
    let has_perms_input = permissions_config
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let has_scripts = hook_scripts
        .as_ref()
        .map(|v| !v.is_empty())
        .unwrap_or(false);

    if hooks_status.attempted && hooks_status.error.is_some() {
        tracing::error!(
            error = ?hooks_status.error,
            "Invalid hooksConfig, keeping existing hook configs"
        );
    }

    let (mcp_servers, should_update_mcp) = if has_mcp_input {
        match serde_json::from_str::<Value>(mcp_servers_config.as_deref().unwrap_or("")) {
            Ok(v) => (Some(v), true),
            Err(e) => {
                tracing::warn!(error = %e, "Failed to parse mcpServersConfig, keeping existing .mcp.json");
                (None, false)
            }
        }
    } else {
        (None, false)
    };

    let (permissions, should_update_perms) = if has_perms_input {
        match serde_json::from_str::<Value>(permissions_config.as_deref().unwrap_or("")) {
            Ok(v) => (Some(v), true),
            Err(e) => {
                tracing::warn!(error = %e, "Failed to parse permissionsConfig, keeping existing permissions");
                (None, false)
            }
        }
    } else {
        (None, false)
    };

    let should_update_hooks = hooks_status.attempted && hooks_status.error.is_none();
    let should_update_scripts = has_scripts;
    let install_platform_env = has_platform_env_script(hook_scripts.as_deref());

    if !should_update_mcp && !should_update_hooks && !should_update_perms && !should_update_scripts
    {
        return Ok(());
    }

    let mut staging_root: Option<PathBuf> = None;
    let result = apply_hook_configs(
        workspace,
        ApplyFlags {
            should_update_mcp,
            should_update_hooks,
            should_update_perms,
            should_update_scripts,
            install_platform_env,
        },
        &mcp_servers,
        &hooks_status.hooks_map,
        &permissions,
        hook_scripts.as_deref(),
        &mut staging_root,
    )
    .await;

    // finally: 清理 staging 目录 (无论成功失败)
    if let Some(root) = staging_root {
        let _ = fs::remove_dir_all(&root).await;
    }

    // nuwax: catch 后仅记录, 不向上抛 (best-effort, 不破坏 workspace 创建)
    if let Err(e) = result {
        tracing::error!(
            error = %e,
            "Failed to write agent hook configs, keeping previous files when possible"
        );
    }
    Ok(())
}

struct ApplyFlags {
    should_update_mcp: bool,
    should_update_hooks: bool,
    should_update_perms: bool,
    should_update_scripts: bool,
    install_platform_env: bool,
}

/// 实际 FS 写入逻辑 (对齐 nuwax writeAgentHookConfigs 的 try 块)。
/// `staging_out` 在创建 staging 时回填, 供外层 finally 清理 (即便中途 `?` 提前返回)。
#[allow(clippy::too_many_arguments)]
async fn apply_hook_configs(
    workspace: &Path,
    flags: ApplyFlags,
    mcp_servers: &Option<Value>,
    hooks_map: &Option<Map<String, Value>>,
    permissions: &Option<Value>,
    hook_scripts: Option<&[HookScript]>,
    staging_out: &mut Option<PathBuf>,
) -> AppResult<()> {
    let claude_dir = workspace.join(".claude");
    let settings_path = claude_dir.join("settings.json");
    let mut staged_runtime: Option<StagedRuntime> = None;

    fs::create_dir_all(&claude_dir).await?;

    // 1. staging 预生成 (仅 hooks 有效且 hooksMap 非空时)
    if flags.should_update_hooks
        && let Some(hm) = hooks_map
    {
        let staged =
            stage_runtime_hook_artifacts(workspace, hm, flags.install_platform_env).await?;
        *staging_out = Some(staged.staging_root.clone());
        staged_runtime = Some(staged);
    }

    // 2. .mcp.json
    if flags.should_update_mcp
        && let Some(mcp) = mcp_servers
    {
        write_json_file_atomic(&workspace.join(".mcp.json"), &json!({ "mcpServers": mcp })).await?;
        tracing::info!("Written .mcp.json to workspace root");
    }

    // 3. 应用 / 清理 Codex+OpenCode 运行时产物
    if flags.should_update_hooks && hooks_map.is_some() {
        if let Some(staged) = staged_runtime.as_ref() {
            apply_staged_runtime_hook_artifacts(workspace, staged).await?;
        }
    } else if flags.should_update_hooks && hooks_map.is_none() {
        clear_hook_artifacts(workspace).await?;
    }

    // 4. .claude/settings.json (hooks + permissions)
    if flags.should_update_hooks || flags.should_update_perms {
        let (settings, corrupt) = read_claude_settings(&settings_path).await;
        if corrupt && !flags.should_update_hooks {
            tracing::warn!(settings = %settings_path.display(), "Corrupt .claude/settings.json, skipping settings update");
        } else {
            if flags.should_update_perms && corrupt {
                tracing::warn!(
                    settings = %settings_path.display(),
                    "Corrupt .claude/settings.json, rewriting hooks without preserving old fields"
                );
            }
            let mut next_settings = if corrupt {
                Map::new()
            } else {
                settings.as_object().cloned().unwrap_or_default()
            };
            if flags.should_update_hooks {
                if let Some(hm) = hooks_map {
                    next_settings.insert("hooks".to_string(), Value::Object(hm.clone()));
                } else {
                    next_settings.remove("hooks");
                }
            }
            if flags.should_update_perms
                && !corrupt
                && let Some(perms) = permissions
            {
                next_settings.insert("permissions".to_string(), perms.clone());
            }
            if !next_settings.is_empty() {
                write_json_file_atomic(&settings_path, &Value::Object(next_settings)).await?;
                tracing::info!("Written .claude/settings.json");
            } else if fs::try_exists(&settings_path).await.unwrap_or(false) {
                let _ = fs::remove_file(&settings_path).await;
            }
        }
    }

    // 5. hook 外挂脚本 + 可选 platform-env 插件
    if flags.should_update_scripts {
        let claude_hooks_dir = claude_dir.join("hooks");
        if fs::try_exists(&claude_hooks_dir).await.unwrap_or(false) {
            let _ = fs::remove_dir_all(&claude_hooks_dir).await;
        }
        if let Some(scripts) = hook_scripts {
            write_hook_scripts(&claude_dir, scripts).await?;
        }
        if flags.install_platform_env {
            let opencode_plugins_dir = workspace.join(".opencode").join("plugins");
            let _ = install_opencode_platform_env_plugin(&opencode_plugins_dir).await?;
        }
    }

    Ok(())
}

// ── 测试 ─────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hooks_status_empty() {
        let s = parse_hooks_config_with_status(None);
        assert!(!s.attempted);
        let s = parse_hooks_config_with_status(Some("   "));
        assert!(!s.attempted);
    }

    #[test]
    fn parse_hooks_status_invalid_records_error() {
        let s = parse_hooks_config_with_status(Some("{not json"));
        assert!(s.attempted);
        assert!(s.error.is_some());
        assert!(s.hooks_map.is_none());
    }

    #[test]
    fn parse_hooks_status_valid_normalizes() {
        let input =
            r#"{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"echo hi"}]}]}"#;
        let s = parse_hooks_config_with_status(Some(input));
        assert!(s.attempted);
        assert!(s.error.is_none());
        let hm = s.hooks_map.expect("hooks map");
        assert!(hm.contains_key("PreToolUse"));
    }

    #[test]
    fn normalize_hooks_map_drops_empty_groups() {
        // hooks 为空数组 → 整组丢弃
        let v: Value =
            serde_json::from_str(r#"{"PreToolUse":[{"matcher":"*","hooks":[]}]}"#).unwrap();
        assert!(normalize_hooks_map(&v).is_none());
    }

    #[test]
    fn normalize_hooks_map_parses_string_handler() {
        // handler 是 JSON 字符串 → 解析为 object
        let v: Value = serde_json::from_str(
            r#"{"PreToolUse":[{"hooks":["{\"type\":\"command\",\"command\":\"x\"}"]}]}"#,
        )
        .unwrap();
        let hm = normalize_hooks_map(&v).expect("some");
        let groups = hm.get("PreToolUse").unwrap().as_array().unwrap();
        let handlers = groups[0]
            .as_object()
            .unwrap()
            .get("hooks")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(handlers.len(), 1);
        assert_eq!(
            handlers[0].get("type").and_then(|v| v.as_str()),
            Some("command")
        );
    }

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

    #[test]
    fn has_platform_env_script_detect() {
        let scripts = vec![HookScript {
            path: "hooks/platform-env.sh".to_string(),
            content: "".to_string(),
        }];
        assert!(has_platform_env_script(Some(&scripts)));
        let other = vec![HookScript {
            path: "hooks/other.sh".to_string(),
            content: "".to_string(),
        }];
        assert!(!has_platform_env_script(Some(&other)));
        assert!(!has_platform_env_script(None));
    }

    #[tokio::test]
    async fn write_agent_hook_configs_end_to_end() {
        let tmp = std::env::temp_dir().join(format!("fs_hook_{}", now_nanos()));
        fs::create_dir_all(&tmp).await.unwrap();

        let hooks = r#"{"PreToolUse":[{"matcher":"Write","hooks":[{"type":"command","command":"./check.sh"}]},{"hooks":[{"type":"http","url":"https://hook.example.com/cb","timeout":5,"headers":{"X-Token":"$SECRET"}}]}]}"#;
        let mcp = r#"{"filesystem":{"command":"npx","args":["-y","@fs/mcp"]}}"#;
        let perms = r#"{"allow":["Bash(echo:*)"],"deny":[]}"#;
        let scripts = vec![
            HookScript {
                path: "hooks/check.sh".to_string(),
                content: "#!/usr/bin/env bash\necho check\n".to_string(),
            },
            HookScript {
                path: "hooks/platform-env.sh".to_string(),
                content: "#!/usr/bin/env bash\necho env\n".to_string(),
            },
        ];

        write_agent_hook_configs(
            &tmp,
            HookConfigInput {
                mcp_servers_config: Some(mcp.to_string()),
                hooks_config: Some(hooks.to_string()),
                permissions_config: Some(perms.to_string()),
                hook_scripts: Some(scripts),
            },
        )
        .await
        .unwrap();

        // .mcp.json
        let mcp_data: Value =
            serde_json::from_str(&fs::read_to_string(tmp.join(".mcp.json")).await.unwrap())
                .unwrap();
        assert_eq!(
            mcp_data
                .get("mcpServers")
                .unwrap()
                .get("filesystem")
                .unwrap()
                .get("command"),
            Some(&json!("npx"))
        );

        // .claude/settings.json: hooks + permissions
        let settings: Value = serde_json::from_str(
            &fs::read_to_string(tmp.join(".claude").join("settings.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(settings.get("hooks").unwrap().get("PreToolUse").is_some());
        assert!(settings.get("permissions").unwrap().get("allow").is_some());

        // .codex/hooks.json: http 转 command wrapper
        let codex: Value = serde_json::from_str(
            &fs::read_to_string(tmp.join(".codex").join("hooks.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        let handlers = codex["hooks"]["PreToolUse"][1]["hooks"].as_array().unwrap();
        assert_eq!(handlers[0]["type"], "command");
        assert!(
            handlers[0]["command"]
                .as_str()
                .unwrap()
                .contains("http-hook-0.sh")
        );
        // wrapper 脚本写入 .codex/hooks/
        assert!(
            tmp.join(".codex")
                .join("hooks")
                .join("http-hook-0.sh")
                .is_file()
        );

        // hook 外挂脚本 (含路径校验, 0o755)
        let check = tmp.join(".claude").join("hooks").join("check.sh");
        assert!(check.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&check).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }

        // opencode 插件 (platform-env 因含 platform-env.sh 触发)
        assert!(
            tmp.join(".opencode")
                .join("plugins")
                .join(OPENCODE_PLUGIN_ENTRY)
                .is_file()
        );
        assert!(
            tmp.join(".opencode")
                .join("plugins")
                .join(OPENCODE_PLUGIN_DIR)
                .join("dist")
                .join("index.js")
                .is_file()
        );
        assert!(
            tmp.join(".opencode")
                .join("plugins")
                .join(OPENCODE_PLATFORM_ENV_PLUGIN_ENTRY)
                .is_file()
        );

        // staging 目录已清理
        let tmp_dir = tmp.join(".tmp");
        if tmp_dir.exists() {
            let remaining: Vec<_> = std::fs::read_dir(&tmp_dir).unwrap().collect();
            assert!(remaining.is_empty(), "staging dir not cleaned");
        }

        let _ = fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn write_agent_hook_configs_invalid_hooks_keeps_existing() {
        let tmp = std::env::temp_dir().join(format!("fs_hook2_{}", now_nanos()));
        fs::create_dir_all(&tmp).await.unwrap();
        // 预置旧 .mcp.json
        fs::write(
            tmp.join(".mcp.json"),
            "{\"mcpServers\":{\"old\":{\"command\":\"x\"}}}\n",
        )
        .await
        .unwrap();

        // 无效 hooksConfig + 有效 mcp: 应更新 mcp, 不动 hooks 产物
        write_agent_hook_configs(
            &tmp,
            HookConfigInput {
                mcp_servers_config: Some(r#"{"new":{"command":"y"}}"#.to_string()),
                hooks_config: Some("{bad json".to_string()),
                permissions_config: None,
                hook_scripts: None,
            },
        )
        .await
        .unwrap();

        let mcp: Value =
            serde_json::from_str(&fs::read_to_string(tmp.join(".mcp.json")).await.unwrap())
                .unwrap();
        assert!(mcp["mcpServers"]["new"].is_object());
        // 旧 entry 被覆盖 (mcp 是整文件重写, 对齐 nuwax)
        assert!(mcp["mcpServers"]["old"].is_null());
        // 无 .codex/hooks.json (hooks 无效未触发)
        assert!(!tmp.join(".codex").join("hooks.json").exists());

        let _ = fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn write_agent_hook_configs_traversal_script_skipped() {
        let tmp = std::env::temp_dir().join(format!("fs_hook3_{}", now_nanos()));
        fs::create_dir_all(&tmp).await.unwrap();
        write_agent_hook_configs(
            &tmp,
            HookConfigInput {
                hook_scripts: Some(vec![HookScript {
                    path: "../escape.sh".to_string(),
                    content: "pwn".to_string(),
                }]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!tmp.parent().unwrap().join("escape.sh").exists());
        let _ = fs::remove_dir_all(&tmp).await;
    }
}
