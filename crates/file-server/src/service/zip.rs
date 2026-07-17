//! Zip 解压/打包 (对齐 nuwax `zipUtils.extractZip` + `archiver`)。
//!
//! - 解压在 `spawn_blocking` 中执行 (zip crate 同步 IO), 不阻塞 async runtime;
//!   每个 entry 经 [`crate::path_safety::safe_zip_entry`] 校验 (Zip Slip 防御)。
//! - 打包同理 (备份/export/download), 支持排除目录/文件名 + 符号链接/硬链接过滤。

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

/// 打包过滤选项 (对齐 nuwax `backupProjectToZip` vs `downloadAllFiles` 两套过滤强度)。
#[derive(Clone, Default)]
pub struct PackOpts {
    pub exclude_dirs: Vec<String>,
    pub exclude_files: Vec<String>,
    /// 跳过任意以 `.` 开头的路径段 (downloadAllFiles 的 dot-segment 过滤)。
    pub skip_dot_segments: bool,
    /// 跳过硬链接 (nlink>1, 仅 downloadAllFiles)。
    pub skip_hardlinks: bool,
}

/// 异步把 `src` 目录打包成 zip 到 `zip_path` (备份/export 用弱过滤: 仅排除名 + 符号链接;
/// 对齐 nuwax backupProjectToZip)。
pub async fn pack_dir(
    src: PathBuf,
    zip_path: PathBuf,
    exclude_dirs: Vec<String>,
    exclude_files: Vec<String>,
) -> AppResult<()> {
    pack_with_opts(
        src,
        zip_path,
        PackOpts {
            exclude_dirs,
            exclude_files,
            skip_dot_segments: false,
            skip_hardlinks: false,
        },
    )
    .await
}

/// 异步打包 (download/computer 用强过滤: dot-segment + 符号链接 + 硬链接;
/// 对齐 nuwax downloadAllFiles entry filter)。
pub async fn pack_download(src: PathBuf, zip_path: PathBuf, opts: PackOpts) -> AppResult<()> {
    let mut o = opts;
    o.skip_dot_segments = true;
    o.skip_hardlinks = true;
    pack_with_opts(src, zip_path, o).await
}

async fn pack_with_opts(src: PathBuf, zip_path: PathBuf, opts: PackOpts) -> AppResult<()> {
    tokio::task::spawn_blocking(move || pack_blocking(&src, &zip_path, &opts))
        .await
        .map_err(|e| AppError::system(format!("zip pack task join error: {e}")))??;
    Ok(())
}

fn pack_blocking(src: &Path, zip_path: &Path, opts: &PackOpts) -> AppResult<()> {
    if let Some(parent) = zip_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::File::create(zip_path)
        .map_err(|e| AppError::file(format!("create zip failed: {e}")))?;
    let mut zip = zip::ZipWriter::new(file);
    walk_and_add(src, src, &mut zip, opts)?;
    zip.finish()
        .map_err(|e| AppError::file(format!("zip finish failed: {e}")))?;
    Ok(())
}

/// 递归遍历并加入 zip (对齐 nuwax archiver.directory 的 entry filter)。
/// 符号链接一律跳过 (lstat); dot-segment/硬链接按 opts; 排除名按 opts。
fn walk_and_add(
    root: &Path,
    dir: &Path,
    zip: &mut zip::ZipWriter<std::fs::File>,
    opts: &PackOpts,
) -> AppResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        // dot-segment 过滤: 任意以 `.` 开头的段 (downloadAllFiles)
        if opts.skip_dot_segments && name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        // 用 symlink_metadata (lstat) 探测真实类型, 拒绝跟随符号链接 (对齐 nuwax lstatSync)
        let meta = std::fs::symlink_metadata(&path).map_err(|e| {
            AppError::system(format!("read metadata {}: {e}", path.display()))
        })?;
        // 符号链接一律跳过 (对齐 nuwax isSymbolicLink)
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            if opts.exclude_dirs.iter().any(|d| d == &name) {
                continue;
            }
            walk_and_add(root, &path, zip, opts)?;
        } else if meta.is_file() {
            if opts.exclude_files.iter().any(|f| f == &name) {
                continue;
            }
            // 硬链接跳过 (nlink>1, 仅 downloadAllFiles)
            if opts.skip_hardlinks && hardlinked(&meta) {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or_else(|_| Path::new(""))
                .to_string_lossy()
                .replace('\\', "/");
            let opts_zip = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file(&rel, opts_zip)
                .map_err(|e| AppError::file(format!("zip start_file failed: {e}")))?;
            let mut input = std::fs::File::open(&path)?;
            std::io::copy(&mut input, zip)?;
        }
    }
    Ok(())
}

/// 是否硬链接 (nlink>1, unix)。
fn hardlinked(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.nlink() > 1
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}
