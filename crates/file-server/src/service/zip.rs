//! Zip 解压/打包 (对齐 nuwax `zipUtils.extractZip` + `archiver`)。
//!
//! 解压在 `spawn_blocking` 中执行 (zip crate 为同步 IO), 不阻塞 async runtime。
//! 每个 entry 经 [`crate::path_safety::safe_zip_entry`] 校验 (Zip Slip 防御)。

use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};
use crate::path_safety::safe_zip_entry;

/// 异步解压 `zip_path` 到 `dst`。
pub async fn extract_to(zip_path: PathBuf, dst: PathBuf) -> AppResult<()> {
    tokio::task::spawn_blocking(move || extract_blocking(&zip_path, &dst))
        .await
        .map_err(|e| AppError::system(format!("zip extract task join error: {e}")))??;
    Ok(())
}

fn extract_blocking(zip_path: &Path, dst: &Path) -> AppResult<()> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| AppError::file(format!("zip open failed: {e}")))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| AppError::file(format!("zip parse failed: {e}")))?;
    std::fs::create_dir_all(dst)?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| AppError::file(format!("zip entry {i} read failed: {e}")))?;
        let name = entry.name().to_string();
        let target = match safe_zip_entry(dst, &name) {
            Ok(p) => p,
            Err(_) => {
                // 对齐 nuwax: 不安全 entry 警告后跳过, 不中止整批
                tracing::warn!(entry = %name, "skip unsafe zip entry");
                continue;
            }
        };
        if entry.is_dir() {
            std::fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&target)
                .map_err(|e| AppError::file(format!("create file failed: {e}")))?;
            std::io::copy(&mut entry, &mut out)?;
        }
    }
    Ok(())
}
