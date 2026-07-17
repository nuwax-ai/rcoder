//! hook 外挂脚本写入 (对齐 nuwax `writeHookScripts`)。
//!
//! 写入 `.claude/hooks/` 下外挂脚本 (路径校验防穿越, 0o755)。

use std::path::Path;

use tokio::fs;

use crate::error::AppResult;

use super::io_util::write_file_atomic;
use super::types::HookScript;

/// 写入 hook 外挂脚本 (相对 .claude 目录; 路径校验防穿越, 0o755)。
/// 对齐 nuwax writeHookScripts: path.normalize 后 starts_with("..") 或 isAbsolute → 跳过。
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
