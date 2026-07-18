//! Zip 解压/打包 (对齐 nuwax `zipUtils.extractZip` + `archiver`)。
//!
//! - 解压在 `spawn_blocking` 中执行 (zip crate 同步 IO), 不阻塞 async runtime;
//!   每个 entry 经 [`crate::path_safety::safe_zip_entry`] 校验 (Zip Slip 防御)。
//! - 打包同理 (备份/export/download), 支持排除目录/文件名 + 符号链接/硬链接过滤。

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};
use crate::path_safety::safe_zip_entry;

/// 防止压缩炸弹耗尽 Pod 临时盘。上传文件本身最大 1 GiB；解压后允许最多 4 GiB，
/// 但任一文件仍不得超过 1 GiB。
const MAX_EXTRACTED_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_EXTRACTED_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ZIP_ENTRY_COUNT: usize = 100_000;

#[derive(Clone, Copy)]
struct ExtractionLimits {
    total_bytes: u64,
    file_bytes: u64,
    entry_count: usize,
}

const EXTRACTION_LIMITS: ExtractionLimits = ExtractionLimits {
    total_bytes: MAX_EXTRACTED_TOTAL_BYTES,
    file_bytes: MAX_EXTRACTED_FILE_BYTES,
    entry_count: MAX_ZIP_ENTRY_COUNT,
};

/// 异步解压 `zip_path` 到 `dst`。
pub async fn extract_to(zip_path: PathBuf, dst: PathBuf) -> AppResult<()> {
    tokio::task::spawn_blocking(move || extract_blocking(&zip_path, &dst))
        .await
        .map_err(|e| AppError::system(format!("zip extract task join error: {e}")))??;
    Ok(())
}

fn extract_blocking(zip_path: &Path, dst: &Path) -> AppResult<()> {
    extract_blocking_with_limits(zip_path, dst, EXTRACTION_LIMITS)
}

fn extract_blocking_with_limits(
    zip_path: &Path,
    dst: &Path,
    limits: ExtractionLimits,
) -> AppResult<()> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| AppError::file(format!("zip open failed: {e}")))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| AppError::file(format!("zip parse failed: {e}")))?;
    if archive.len() > limits.entry_count {
        return Err(AppError::validation(format!(
            "zip contains too many entries (max {})",
            limits.entry_count
        )));
    }
    std::fs::create_dir_all(dst)?;
    let mut extracted_bytes = 0_u64;
    for i in 0..archive.len() {
        let entry = archive
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
            if entry.size() > limits.file_bytes {
                return Err(AppError::validation(format!(
                    "zip entry {name} exceeds extracted file limit (max {} bytes)",
                    limits.file_bytes
                )));
            }
            let remaining = limits
                .total_bytes
                .checked_sub(extracted_bytes)
                .ok_or_else(|| AppError::validation("zip extracted size exceeds limit"))?;
            if entry.size() > remaining {
                return Err(AppError::validation(format!(
                    "zip extracted size exceeds limit (max {} bytes)",
                    limits.total_bytes
                )));
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&target)
                .map_err(|e| AppError::file(format!("create file failed: {e}")))?;
            // 不只信任 central directory 中的 entry.size()；按实际解压字节再次限流。
            let copy_limit = remaining
                .min(limits.file_bytes)
                .checked_add(1)
                .ok_or_else(|| AppError::validation("zip extraction limit overflow"))?;
            let copied = std::io::copy(&mut entry.take(copy_limit), &mut out)?;
            if copied >= copy_limit {
                return Err(AppError::validation(format!(
                    "zip entry {name} or extracted total exceeds size limit"
                )));
            }
            extracted_bytes = extracted_bytes
                .checked_add(copied)
                .ok_or_else(|| AppError::validation("zip extracted size overflow"))?;
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
    /// 每个 entry 名前缀 (downloadAllFiles 的 `${userId}_${cId}/` 顶层目录前缀)。
    pub path_prefix: Option<String>,
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
            path_prefix: None,
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

/// 异步打包 (自定义 opts; 供 zip-workspace 等需要弱过滤、但无 dot-segment 过滤的场景)。
pub async fn pack_with_opts(src: PathBuf, zip_path: PathBuf, opts: PackOpts) -> AppResult<()> {
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
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| AppError::system(format!("read metadata {}: {e}", path.display())))?;
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
            // entry 名加 path_prefix (downloadAllFiles 顶层目录前缀)
            let entry_name = match &opts.path_prefix {
                Some(p) => format!("{p}{rel}"),
                None => rel,
            };
            let opts_zip = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file(&entry_name, opts_zip)
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

/// 按 opts 过滤规则求目录下可下载文件总字节 (对齐 nuwax calculateDownloadableDirectorySize;
/// dot-segment + 符号链接 + 硬链接 + 排除名)。同步 IO, 调用方宜 spawn_blocking。
pub fn downloadable_size_blocking(src: &Path, opts: &PackOpts) -> u64 {
    sum_sizes(src, opts)
}

fn sum_sizes(dir: &Path, opts: &PackOpts) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if opts.skip_dot_segments && name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            if opts.exclude_dirs.iter().any(|d| d == &name) {
                continue;
            }
            total += sum_sizes(&path, opts);
        } else if meta.is_file() {
            if opts.exclude_files.iter().any(|f| f == &name) {
                continue;
            }
            if opts.skip_hardlinks && hardlinked(&meta) {
                continue;
            }
            total += meta.len();
        }
    }
    total
}

/// 异步写一个仅含单个目录条目 `dir_entry_name/` 的空 zip (downloadAllFiles 空目录兜底)。
pub async fn write_empty_zip(zip_path: PathBuf, dir_entry_name: String) -> AppResult<()> {
    tokio::task::spawn_blocking(move || -> AppResult<()> {
        if let Some(parent) = zip_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(&zip_path)
            .map_err(|e| AppError::file(format!("create empty zip failed: {e}")))?;
        let mut zip = zip::ZipWriter::new(file);
        let name = if dir_entry_name.ends_with('/') {
            dir_entry_name
        } else {
            format!("{dir_entry_name}/")
        };
        let opts = zip::write::SimpleFileOptions::default();
        zip.add_directory(&name, opts)
            .map_err(|e| AppError::file(format!("zip add_directory failed: {e}")))?;
        zip.finish()
            .map_err(|e| AppError::file(format!("zip finish failed: {e}")))?;
        Ok(())
    })
    .await
    .map_err(|e| AppError::system(format!("empty zip task join error: {e}")))??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn unique_tmp(prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("fs_{prefix}_{nanos}"))
    }

    /// 构造工作区树: src/index.js + node_modules/pkg.js + dist/index.html + .gitignore + package-lock.yaml。
    fn make_tree(root: &Path) {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(root.join("src/index.js"), "x").unwrap();
        fs::write(root.join("node_modules/pkg.js"), "x").unwrap();
        fs::write(root.join("dist/index.html"), "x").unwrap();
        fs::write(root.join(".gitignore"), "node_modules").unwrap();
        fs::write(root.join("package-lock.yaml"), "x").unwrap();
    }

    fn entry_names(zip_path: &Path) -> Vec<String> {
        let f = fs::File::open(zip_path).unwrap();
        let mut z = zip::ZipArchive::new(f).unwrap();
        (0..z.len())
            .filter_map(|i| z.by_index(i).ok().map(|e| e.name().to_string()))
            .collect()
    }

    #[test]
    fn extraction_rejects_actual_content_over_limit() {
        let source = unique_tmp("zip_limit_source");
        let destination = unique_tmp("zip_limit_destination");
        let file = fs::File::create(&source).expect("create zip fixture");
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("large.txt", zip::write::SimpleFileOptions::default())
            .expect("start zip entry");
        writer.write_all(b"two bytes").expect("write zip entry");
        writer.finish().expect("finish zip fixture");

        let result = extract_blocking_with_limits(
            &source,
            &destination,
            ExtractionLimits {
                total_bytes: 1,
                file_bytes: 1,
                entry_count: 1,
            },
        );

        assert!(result.is_err());
        let _ = fs::remove_file(source);
        let _ = fs::remove_dir_all(destination);
    }

    /// download-all-files 口径: traverse_exclude_dirs + content excludes + dot-segment 过滤。
    /// 锁定 P0 #1: node_modules/dist 目录被排除 + dot 文件(.gitignore)/lock 被排除。
    #[test]
    fn download_excludes_dirs_dotfiles_and_locks() {
        let src = unique_tmp("zipsrc_dl");
        let out = unique_tmp("zipout_dl");
        make_tree(&src);
        let opts = PackOpts {
            exclude_dirs: vec!["node_modules".into(), "dist".into()],
            exclude_files: vec!["package-lock.yaml".into()],
            skip_dot_segments: true,
            skip_hardlinks: false,
            path_prefix: Some("u_c/".into()),
        };
        let _ = pack_blocking(&src, &out, &opts);
        let names = entry_names(&out);
        assert!(names.contains(&"u_c/src/index.js".to_string()), "{names:?}");
        assert!(
            !names.iter().any(|n| n.contains("node_modules")),
            "{names:?}"
        );
        assert!(!names.iter().any(|n| n.contains("dist/")), "{names:?}");
        assert!(!names.iter().any(|n| n.contains(".gitignore")), "{names:?}");
        assert!(
            !names.iter().any(|n| n.contains("package-lock.yaml")),
            "{names:?}"
        );
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_file(&out);
    }

    /// zip-workspace 口径: 合并集填 dirs+files, **无** dot-segment 过滤。
    /// 锁定 P0 #2: .gitignore 被保留 (非 pack_download), node_modules/dist 仍排除。
    #[test]
    fn workspace_keeps_gitignore_excludes_dirs() {
        let src = unique_tmp("zipsrc_ws");
        let out = unique_tmp("zipout_ws");
        make_tree(&src);
        let merged = vec!["node_modules".into(), "dist".into(), ".git".into()];
        let opts = PackOpts {
            exclude_dirs: merged.clone(),
            exclude_files: merged,
            skip_dot_segments: false,
            skip_hardlinks: false,
            path_prefix: None,
        };
        let _ = pack_blocking(&src, &out, &opts);
        let names = entry_names(&out);
        assert!(names.contains(&"src/index.js".to_string()), "{names:?}");
        assert!(names.contains(&".gitignore".to_string()), "{names:?}");
        assert!(
            !names.iter().any(|n| n.contains("node_modules")),
            "{names:?}"
        );
        assert!(!names.iter().any(|n| n.contains("dist/")), "{names:?}");
        let _ = fs::remove_dir_all(&src);
        let _ = fs::remove_file(&out);
    }
}
