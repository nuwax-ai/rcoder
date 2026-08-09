//! 原子文件写入辅助 (对齐 nuwax `writeFileAtomic` / `writeJsonFileAtomic`)。
//!
//! 供 agent_hooks 各子模块复用: staging 写 codex 脚本 / settings.json / hook 脚本等。

use std::io::Write;
use std::path::Path;

use serde_json::Value;
use tokio::fs;

use crate::error::{AppError, AppResult};

/// 原子写文本文件：在目标目录创建不可预测的临时文件，再原子替换目标。
///
/// `tempfile` 负责排他创建与异常路径清理；阻塞文件操作放入 blocking 线程池。
pub(super) async fn write_file_atomic(
    target: &Path,
    content: &str,
    mode: Option<u32>,
) -> AppResult<()> {
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).await?;
    let dir = dir.to_path_buf();
    let target = target.to_path_buf();
    let content = content.as_bytes().to_vec();
    tokio::task::spawn_blocking(move || -> AppResult<()> {
        let mut temporary = tempfile::Builder::new()
            .prefix(".hook-config-")
            .tempfile_in(&dir)?;
        temporary.write_all(&content)?;
        temporary.flush()?;
        if let Some(mode) = mode {
            set_mode(temporary.as_file(), mode)?;
        }
        temporary.as_file().sync_all()?;
        temporary.persist(&target).map_err(|error| error.error)?;
        Ok(())
    })
    .await
    .map_err(|error| AppError::system(format!("join atomic file writer: {error}")))?
}

/// 原子写 JSON 文件 (对齐 nuwax writeJsonFileAtomic: pretty + 末尾换行)。
pub(super) async fn write_json_file_atomic(target: &Path, data: &Value) -> AppResult<()> {
    let mut s = serde_json::to_string_pretty(data)
        .map_err(|e| AppError::system(format!("serialize json: {e}")))?;
    s.push('\n');
    write_file_atomic(target, &s, None).await
}

/// 删除文件、目录或符号链接；不存在视为成功，其他 I/O 错误立即返回。
pub(super) async fn remove_path_if_exists(path: &Path) -> AppResult<()> {
    let metadata = match fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path).await?;
    } else {
        fs::remove_file(path).await?;
    }
    Ok(())
}

/// 设置文件权限 (unix only; 0o755 等)。
#[cfg(unix)]
fn set_mode(file: &std::fs::File, mode: u32) -> AppResult<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_mode(_file: &std::fs::File, _mode: u32) -> AppResult<()> {
    Ok(())
}
