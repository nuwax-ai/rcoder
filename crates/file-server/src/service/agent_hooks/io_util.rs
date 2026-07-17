//! 原子文件写入辅助 (对齐 nuwax `writeFileAtomic` / `writeJsonFileAtomic`)。
//!
//! 供 agent_hooks 各子模块复用: staging 写 codex 脚本 / settings.json / hook 脚本等。

use std::path::Path;

use serde_json::Value;
use tokio::fs;

use crate::error::{AppError, AppResult};

/// 当前时间纳秒 (用于生成唯一临时名 / staging 目录名)。
pub(super) fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// 原子写文本文件: 先写 `.<name>.<pid>.<nanos>.tmp` 再 rename (对齐 nuwax writeFileAtomic)。
pub(super) async fn write_file_atomic(
    target: &Path,
    content: &str,
    mode: Option<u32>,
) -> AppResult<()> {
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
pub(super) async fn write_json_file_atomic(target: &Path, data: &Value) -> AppResult<()> {
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
