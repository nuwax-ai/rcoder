use std::collections::{HashMap, HashSet};
use std::path::Path;

#[cfg(windows)]
use crate::path_env::TAURI_APP_DATA_DIR;
use shared_types::{ModelEnvBinding, ModelEnvBindingSource};
use tracing::{debug, info, warn};

use super::super::model_env::ResolvedModelEnv;
use super::types::{
    ENV_ANTHROPIC_API_KEY, ENV_ANTHROPIC_BASE_URL, ENV_CODEX_API_KEY, ENV_CODEX_BASE_URL,
    ENV_OPENAI_API_KEY, ENV_OPENAI_BASE_URL,
};

/// 获取各平台下常用的用户二进制目录
///
/// Linux/Mac:
/// - ~/.cargo/bin (Rust cargo)
/// - ~/.npm-global/bin (npm global)
/// - ~/.local/bin (uv, pipx)
/// - /opt/homebrew/bin (Homebrew on Apple Silicon)
/// - /usr/local/bin (Homebrew on Intel Mac / 通用)
/// - /home/linuxbrew/.linuxbrew/bin (Linuxbrew)
///
/// Windows:
/// - 返回空，Windows 环境变量已包含这些路径
///
/// 注意：使用 $HOME 变量而非硬编码 /root，HOME 不存在时自动回退到 /root
fn get_common_user_bins() -> Vec<&'static str> {
    #[cfg(windows)]
    {
        vec![] // Windows 用户路径已在系统 PATH 中
    }

    #[cfg(not(windows))]
    {
        vec![
            // Cargo (Rust)
            "$HOME/.cargo/bin",
            // NPM global
            "$HOME/.npm-global/bin",
            // UV / PIPX global
            "$HOME/.local/bin",
            // Homebrew (Apple Silicon)
            "/opt/homebrew/bin",
            // Homebrew (Intel Mac) / 通用本地安装
            "/usr/local/bin",
            // Linuxbrew
            "/home/linuxbrew/.linuxbrew/bin",
            // 系统级 cargo（某些容器环境）
            "/opt/cargo/bin",
        ]
    }
}

/// 确保子进程环境变量中包含 PATH / PATHEXT，便于解析可执行路径与 Windows .cmd 脚本
///
/// PATH 组成（优先级从高到低）：
/// 1. NUWAX_APP_RUNTIME_PATH — 应用自有运行时目录（node/uv/mcp-proxy 等）
/// 2. 当前系统 PATH — 保留所有现有路径（关键改进！）
/// 3. 常用用户目录 — cargo, npm, uv, homebrew 等
/// 4. 系统基础目录 — `/bin`, `/usr/bin` 等（仅在 PATH 为空时）
pub(crate) fn ensure_subprocess_path_env(
    merged_envs: &mut std::collections::HashMap<String, String>,
) {
    if !merged_envs.contains_key("PATH") {
        let path = build_mcp_server_path_env();
        if !path.is_empty() {
            merged_envs.insert("PATH".to_string(), path);
            debug!("[SACP] 📋 already built PATH (using existing PATH directory)");
        }
    }
    #[cfg(windows)]
    ensure_windows_subprocess_env(merged_envs);
}

/// 构建 MCP 服务器子进程所需的 PATH 环境变量
///
/// Claude Code SDK 在启动 MCP 服务器子进程时会用提供的 env 替换整个环境。
/// 如果 env 中缺少 PATH，mcp-proxy convert --config 模式下的孙进程
/// （如 uvx、npx）将因为找不到命令而静默失败。
///
/// 此函数与 ensure_subprocess_path_env 使用相同的逻辑构建 PATH：
/// 1. NUWAX_APP_RUNTIME_PATH — 应用自有运行时目录（优先）
/// 2. 当前系统 PATH — 保留所有现有路径（关键改进！）
/// 3. 平台特定目录：
///    - Windows: APPDATA 推导的 Tauri 应用 node/npm 路径
///    - Unix: 常用用户目录（cargo, npm, uv, homebrew 等）
/// 4. 系统基础目录 — 仅在 PATH 为空时兜底
pub(crate) fn build_mcp_server_path_env() -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    let mut paths: Vec<String> = Vec::new();

    // 1. 优先：NUWAX_APP_RUNTIME_PATH
    if let Ok(runtime_path) = std::env::var("NUWAX_APP_RUNTIME_PATH") {
        for p in runtime_path.split(sep) {
            let p = p.trim();
            if !p.is_empty() && !paths.contains(&p.to_string()) {
                paths.push(p.to_string());
            }
        }
    }

    // 2. 追加：当前系统 PATH（关键改动！）
    if let Ok(current_path) = std::env::var("PATH") {
        for p in current_path.split(sep) {
            let p = p.trim();
            if !p.is_empty() && !paths.contains(&p.to_string()) {
                paths.push(p.to_string());
            }
        }
    }

    // 3. 追加：平台特定目录
    #[cfg(windows)]
    {
        // Windows: 从 APPDATA 推导 Tauri 应用的 node/npm 路径
        if let Ok(appdata) = std::env::var("APPDATA") {
            use std::path::PathBuf;
            let app_base = PathBuf::from(&appdata).join(TAURI_APP_DATA_DIR);

            let node_bin = app_base.join("runtime").join("node").join("bin");
            if node_bin.is_dir() {
                let node_bin_str = node_bin.to_string_lossy().to_string();
                if !paths.contains(&node_bin_str) {
                    paths.push(node_bin_str);
                }
            }

            let npm_bin = app_base.join("node_modules").join(".bin");
            if npm_bin.is_dir() {
                let npm_bin_str = npm_bin.to_string_lossy().to_string();
                if !paths.contains(&npm_bin_str) {
                    paths.push(npm_bin_str);
                }
            }
        }
    }

    #[cfg(not(windows))]
    {
        // Unix: 追加常用用户目录（自动扩展 $HOME）
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        for bin in get_common_user_bins() {
            let expanded = bin.replace("$HOME", &home);
            let path = std::path::Path::new(&expanded);
            if path.is_dir() && !paths.contains(&expanded) {
                paths.push(expanded);
            }
        }
    }

    // 4. 兜底：系统基础目录（仅在 PATH 为空时）
    // 正常情况下不会走到这里，因为步骤 2 已经追加了当前系统 PATH
    // 这是为了防御性编程，确保即使在极端情况下也有可用的基础命令
    if paths.is_empty() {
        #[cfg(not(windows))]
        for sys_dir in &["/bin", "/usr/bin", "/usr/local/bin", "/sbin", "/usr/sbin"] {
            let s = sys_dir.to_string();
            if std::path::Path::new(sys_dir).is_dir() {
                paths.push(s);
            }
        }

        #[cfg(windows)]
        {
            // Windows: 直接返回系统 PATH 作为最后手段
            if let Ok(current_path) = std::env::var("PATH") {
                return current_path;
            }
        }
    }

    paths.join(sep)
}

/// Windows 专属：确保 PATHEXT 存在，以便解析 .cmd/.bat 等脚本
#[cfg(windows)]
fn ensure_windows_subprocess_env(merged_envs: &mut std::collections::HashMap<String, String>) {
    if !merged_envs.contains_key("PATHEXT") {
        if let Ok(pathext) = std::env::var("PATHEXT") {
            merged_envs.insert("PATHEXT".to_string(), pathext);
            debug!("[SACP] 📋 PATHEXT already set");
        }
    }
}

pub(crate) fn render_model_template(value: &mut String, resolved: &ResolvedModelEnv) {
    *value = value
        .replace("{MODEL_PROVIDER_API_KEY}", &resolved.api_key)
        .replace("{MODEL_PROVIDER_BASE_URL}", &resolved.base_url)
        .replace("{MODEL_PROVIDER_DEFAULT_MODEL}", &resolved.default_model)
        .replace("{MODEL_PROVIDER_NAME}", &resolved.provider_name);
}

/// `{PREFIX_WORKSPACE_DIR}` 变量的目标路径
const WORKSPACE_DIR_LOG_PATH: &str = "/app/container-logs";
const WORKSPACE_DIR_HOME_PATH: &str = "/home/user";

/// 使用 Path 规范化路径，去除多余的分隔符和 `.` 组件
///
/// 例如：
/// - `/home/user//project/logs` → `/home/user/project/logs`
/// - `/home/user/./project` → `/home/user/project`
fn normalize_path(path: &str) -> String {
    Path::new(path)
        .components()
        .collect::<std::path::PathBuf>()
        .to_string_lossy()
        .into_owned()
}

/// 解析 `{PREFIX_WORKSPACE_DIR}` 占位符
///
/// 根据不同场景解析为不同的路径：
/// - **环境变量场景**：
///   - `LOG_DIR` / `OPENCODE_LOG_DIR`：解析为 `/app/container-logs`（日志专用目录）
///   - `/devcomputer/chat` 接口的 `LOG_DIR`：解析为 `/home/user`（开发调试方便查看）
/// - **command / args 场景**：解析为 `/home/user`（容器内工作目录前缀）
///
/// # Arguments
/// * `value` - 需要解析的字符串
/// * `env_key` - 环境变量名（用于判断是否是日志相关的变量），command/args 时传 None
/// * `is_devcomputer` - 是否是 devcomputer 接口（影响日志路径解析）
pub(crate) fn render_prefix_workspace_dir(
    value: &mut String,
    env_key: Option<&str>,
    is_devcomputer: bool,
) {
    if !value.contains("{PREFIX_WORKSPACE_DIR}") {
        return;
    }

    let replacement = match env_key {
        Some(key) => {
            // 环境变量场景
            let upper_key = key.to_uppercase();
            if upper_key == "LOG_DIR" || upper_key == "OPENCODE_LOG_DIR" {
                if is_devcomputer {
                    // devcomputer 接口：日志放在项目目录下，方便开发调试
                    // 最终路径: /home/user/{project_id}/.logs
                    WORKSPACE_DIR_HOME_PATH
                } else {
                    // computer 接口：日志放在专用日志目录
                    WORKSPACE_DIR_LOG_PATH
                }
            } else {
                // 其他环境变量：使用 home 路径
                WORKSPACE_DIR_HOME_PATH
            }
        }
        None => {
            // command / args 场景：使用 home 路径
            WORKSPACE_DIR_HOME_PATH
        }
    };

    let replaced = value.replace("{PREFIX_WORKSPACE_DIR}", replacement);
    *value = normalize_path(&replaced);
    debug!(
        "[SACP] Resolved {{PREFIX_WORKSPACE_DIR}} => '{}' (env_key={:?}, is_devcomputer={})",
        replacement, env_key, is_devcomputer
    );
}

fn resolved_model_binding_value(
    resolved: &ResolvedModelEnv,
    source: ModelEnvBindingSource,
) -> &str {
    match source {
        ModelEnvBindingSource::ApiKey => &resolved.api_key,
        ModelEnvBindingSource::BaseUrl => &resolved.base_url,
        ModelEnvBindingSource::DefaultModel => &resolved.default_model,
        ModelEnvBindingSource::ProviderName => &resolved.provider_name,
    }
}

pub(crate) fn apply_model_env_bindings(
    env: &mut HashMap<String, String>,
    bindings: &[ModelEnvBinding],
    resolved: &ResolvedModelEnv,
) -> HashSet<String> {
    let mut bound_keys = HashSet::new();

    for binding in bindings {
        let env_key = binding.env_key.trim();
        if env_key.is_empty() {
            warn!("[SACP] Ignoring model env binding with empty env_key");
            continue;
        }

        let value = resolved_model_binding_value(resolved, binding.source).to_string();
        env.insert(env_key.to_string(), value);
        bound_keys.insert(env_key.to_string());
        debug!(
            "[SACP] Applied model env binding: {} <= {:?}",
            env_key, binding.source
        );
    }

    bound_keys
}

pub(crate) fn apply_sensitive_model_env_fallback(
    env: &mut HashMap<String, String>,
    resolved: &ResolvedModelEnv,
    explicitly_bound_keys: &HashSet<String>,
) {
    if !resolved.override_existing_sensitive_env {
        return;
    }

    info!("[SACP] 🔒 Model env resolver requested sensitive env replacement");
    for key in [
        ENV_ANTHROPIC_API_KEY,
        ENV_OPENAI_API_KEY,
        ENV_CODEX_API_KEY,
        "ANTHROPIC_AUTH_TOKEN",
    ] {
        if env.contains_key(key) && !explicitly_bound_keys.contains(key) {
            env.insert(key.to_string(), resolved.api_key.clone());
            info!("[SACP] 🔒 Replaced {} with resolver-provided API key", key);
        }
    }
    for key in [
        ENV_ANTHROPIC_BASE_URL,
        ENV_OPENAI_BASE_URL,
        ENV_CODEX_BASE_URL,
    ] {
        if env.contains_key(key) && !explicitly_bound_keys.contains(key) {
            env.insert(key.to_string(), resolved.base_url.clone());
            info!("[SACP] 🔒 Replaced {} with: {}", key, resolved.base_url);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_path() {
        // 多余的斜杠被去除
        assert_eq!(normalize_path("/home/user//project"), "/home/user/project");
        assert_eq!(normalize_path("/home/user/project"), "/home/user/project");
        assert_eq!(normalize_path("//home//user//"), "/home/user");

        // 当前目录符号被去除
        assert_eq!(normalize_path("/home/user/./project"), "/home/user/project");

        // 根路径保持不变
        assert_eq!(normalize_path("/"), "/");

        // 空路径
        assert_eq!(normalize_path(""), "");
    }

    #[test]
    fn test_render_prefix_workspace_dir_no_placeholder() {
        let mut value = "/some/path".to_string();
        render_prefix_workspace_dir(&mut value, None, false);
        assert_eq!(value, "/some/path");
    }

    #[test]
    fn test_render_prefix_workspace_dir_command() {
        let mut value = "{PREFIX_WORKSPACE_DIR}/project/node_modules/.bin/tsx".to_string();
        render_prefix_workspace_dir(&mut value, None, false);
        assert_eq!(value, "/home/user/project/node_modules/.bin/tsx");
    }

    #[test]
    fn test_render_prefix_workspace_dir_args() {
        let mut value = "{PREFIX_WORKSPACE_DIR}/project/src/index.ts".to_string();
        render_prefix_workspace_dir(&mut value, None, true);
        assert_eq!(value, "/home/user/project/src/index.ts");
    }

    #[test]
    fn test_render_prefix_workspace_dir_log_dir_computer() {
        let mut value = "{PREFIX_WORKSPACE_DIR}/{conversationId}/logs".to_string();
        render_prefix_workspace_dir(&mut value, Some("LOG_DIR"), false);
        assert_eq!(value, "/app/container-logs/{conversationId}/logs");
    }

    #[test]
    fn test_render_prefix_workspace_dir_log_dir_devcomputer() {
        let mut value = "{PREFIX_WORKSPACE_DIR}/{conversationId}/.logs".to_string();
        render_prefix_workspace_dir(&mut value, Some("LOG_DIR"), true);
        assert_eq!(value, "/home/user/{conversationId}/.logs");
    }

    #[test]
    fn test_render_prefix_workspace_dir_opencode_log_dir_computer() {
        let mut value = "{PREFIX_WORKSPACE_DIR}/{conversationId}/logs".to_string();
        render_prefix_workspace_dir(&mut value, Some("OPENCODE_LOG_DIR"), false);
        assert_eq!(value, "/app/container-logs/{conversationId}/logs");
    }

    #[test]
    fn test_render_prefix_workspace_dir_opencode_log_dir_devcomputer() {
        let mut value = "{PREFIX_WORKSPACE_DIR}/{conversationId}/.logs".to_string();
        render_prefix_workspace_dir(&mut value, Some("OPENCODE_LOG_DIR"), true);
        assert_eq!(value, "/home/user/{conversationId}/.logs");
    }

    #[test]
    fn test_render_prefix_workspace_dir_other_env_var() {
        let mut value = "{PREFIX_WORKSPACE_DIR}/other".to_string();
        render_prefix_workspace_dir(&mut value, Some("OTHER_VAR"), false);
        assert_eq!(value, "/home/user/other");
    }

    #[test]
    fn test_render_prefix_workspace_dir_case_insensitive() {
        let mut value = "{PREFIX_WORKSPACE_DIR}/logs".to_string();
        render_prefix_workspace_dir(&mut value, Some("log_dir"), false);
        assert_eq!(value, "/app/container-logs/logs");

        let mut value = "{PREFIX_WORKSPACE_DIR}/logs".to_string();
        render_prefix_workspace_dir(&mut value, Some("Log_Dir"), true);
        assert_eq!(value, "/home/user/logs");
    }
}
