//! Zip 解压/打包 (对齐 nuwax `zipUtils.extractZip` + `archiver`)。
//!
//! - 解压在 `spawn_blocking` 中执行 (zip crate 同步 IO), 不阻塞 async runtime;
//!   每个 entry 经 [`crate::path_safety::safe_zip_entry`] 校验 (Zip Slip 防御)。
//! - 打包同理 (备份/export), 支持排除目录/文件名。

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
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| AppError::file(format!("zip parse failed: {e}")))?;
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

/// 异步把 `src` 目录打包成 zip 到 `zip_path` (排除指定目录/文件名, 对齐 nuwax backupProjectToZip)。
pub async fn pack_dir(
    src: PathBuf,
    zip_path: PathBuf,
    exclude_dirs: Vec<String>,
    exclude_files: Vec<String>,
) -> AppResult<()> {
    tokio::task::spawn_blocking(move || {
        pack_blocking(&src, &zip_path, &exclude_dirs, &exclude_files)
    })
    .await
    .map_err(|e| AppError::system(format!("zip pack task join error: {e}")))??;
    Ok(())
}

fn pack_blocking(
    src: &Path,
    zip_path: &Path,
    exclude_dirs: &[String],
    exclude_files: &[String],
) -> AppResult<()> {
    if let Some(parent) = zip_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(zip_path)
        .map_err(|e| AppError::file(format!("create zip failed: {e}")))?;
    let mut zip = zip::ZipWriter::new(file);
    walk_and_add(src, src, &mut zip, exclude_dirs, exclude_files)?;
    zip.finish()
        .map_err(|e| AppError::file(format!("zip finish failed: {e}")))?;
    Ok(())
}

fn walk_and_add(
    root: &Path,
    dir: &Path,
    zip: &mut zip::ZipWriter<std::fs::File>,
    exclude_dirs: &[String],
    exclude_files: &[String],
) -> AppResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            if exclude_dirs.iter().any(|d| d == &name) {
                continue;
            }
            walk_and_add(root, &path, zip, exclude_dirs, exclude_files)?;
        } else if ft.is_file() {
            if exclude_files.iter().any(|f| f == &name) {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or_else(|_| Path::new(""))
                .to_string_lossy()
                .replace('\\', "/");
            let opts = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file(&rel, opts)
                .map_err(|e| AppError::file(format!("zip start_file failed: {e}")))?;
            let mut input = std::fs::File::open(&path)?;
            std::io::copy(&mut input, zip)?;
        }
    }
    Ok(())
}
