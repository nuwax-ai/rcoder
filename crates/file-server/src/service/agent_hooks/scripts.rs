//! hook 外挂脚本写入 (对齐 nuwax `writeHookScripts`)。
//!
//! 写入 `.claude/hooks/` 下外挂脚本 (路径校验防穿越, 0o755)。

use std::path::Path;

use tokio::fs;

use crate::error::AppResult;

use super::io_util::write_file_atomic;
use super::types::HookScript;

/// 写入 hook 外挂脚本。协议路径必须位于 `hooks/`，最终目标被限定在
/// `.claude/hooks/` 内，防止脚本输入覆盖 `settings.json`、skills 等配置。
pub(super) async fn write_hook_scripts(
    claude_dir: &Path,
    hook_scripts: &[HookScript],
) -> AppResult<()> {
    if hook_scripts.is_empty() {
        return Ok(());
    }
    let hooks_dir = claude_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).await?;
    for script in hook_scripts {
        if script.path.trim().is_empty() {
            continue;
        }
        let protocol_path = Path::new(script.path.trim());
        let relative = match protocol_path.strip_prefix("hooks") {
            Ok(relative) if !relative.as_os_str().is_empty() => relative,
            _ => {
                tracing::warn!(path = %script.path, "Hook script path must be under hooks/, skipping");
                continue;
            }
        };
        let Some(relative) = relative.to_str() else {
            tracing::warn!(path = %script.path, "Hook script path is not valid UTF-8, skipping");
            continue;
        };
        let target = match crate::path_safety::ensure_within(&hooks_dir, relative) {
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
