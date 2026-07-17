//! OpenCode 插件安装 (对齐 nuwax `installOpencodeHooksPlugin` /
//! `installOpencodePlatformEnvPlugin` + vendored JS 资源)。
//!
//! vendored 插件用 `include_bytes!` 编译进二进制, 运行时写到 `.opencode/plugins/`。
//! 路径相对本文件 (src/service/agent_hooks/opencode.rs) 为 `../../../assets/...`。

use std::path::Path;

use tokio::fs;

use crate::error::AppResult;

use super::io_util::write_file_atomic;
use super::types::HookScript;

pub(super) const OPENCODE_PLUGIN_ENTRY: &str = "opencode-hooks-plugin.js";
pub(super) const OPENCODE_PLUGIN_DIR: &str = "opencode-hooks-plugin";
pub(super) const OPENCODE_PLATFORM_ENV_PLUGIN_ENTRY: &str = "opencode-platform-env-plugin.js";
const PLATFORM_ENV_SCRIPT_PATH: &str = "hooks/platform-env.sh";

// ── vendored opencode 插件 (编译进二进制) ────────────────────────────────────────

/// opencode-hooks-plugin/dist 下所有 .js (对齐 nuwax assets/opencode-hooks-plugin/dist)。
const OPENCODE_HOOKS_PLUGIN_FILES: &[(&str, &[u8])] = &[
    (
        "config.js",
        include_bytes!("../../../assets/opencode-hooks-plugin/dist/config.js"),
    ),
    (
        "events.js",
        include_bytes!("../../../assets/opencode-hooks-plugin/dist/events.js"),
    ),
    (
        "executor.js",
        include_bytes!("../../../assets/opencode-hooks-plugin/dist/executor.js"),
    ),
    (
        "index.js",
        include_bytes!("../../../assets/opencode-hooks-plugin/dist/index.js"),
    ),
    (
        "matcher.js",
        include_bytes!("../../../assets/opencode-hooks-plugin/dist/matcher.js"),
    ),
    (
        "types.js",
        include_bytes!("../../../assets/opencode-hooks-plugin/dist/types.js"),
    ),
];

/// opencode-platform-env-plugin 入口 (对齐 nuwax assets/opencode-platform-env-plugin)。
const OPENCODE_PLATFORM_ENV_PLUGIN_JS: &[u8] =
    include_bytes!("../../../assets/opencode-platform-env-plugin/platform-env-plugin.js");

// ── 安装 ─────────────────────────────────────────────────────────────────────────

/// 安装 vendored opencode-hooks-plugin 到目标 plugins 目录 (对齐 nuwax installOpencodeHooksPlugin)。
/// 写 dist/*.js + 入口 re-export 文件。
pub(super) async fn install_opencode_hooks_plugin(opencode_plugins_dir: &Path) -> AppResult<bool> {
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
pub(super) async fn install_opencode_platform_env_plugin(
    opencode_plugins_dir: &Path,
) -> AppResult<bool> {
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
pub(super) fn has_platform_env_script(hook_scripts: Option<&[HookScript]>) -> bool {
    let Some(scripts) = hook_scripts else {
        return false;
    };
    scripts.iter().any(|s| s.path == PLATFORM_ENV_SCRIPT_PATH)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
