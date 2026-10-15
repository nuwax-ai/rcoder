//! Zip 解压/打包 (对齐 nuwax `zipUtils.extractZip` + `archiver`)。
//!
//! - 解压在 `spawn_blocking` 中执行 (zip crate 同步 IO), 不阻塞 async runtime;
//!   每个 entry 经 [`crate::path_safety::safe_zip_entry`] 校验 (Zip Slip 防御)。
//! - 打包同理 (备份/export/download), 支持排除目录/文件名 + 符号链接/硬链接过滤。

use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::time::Instant;

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
    let start = Instant::now();
    let zip_display = zip_path.display().to_string();
    let dst_display = dst.display().to_string();
    tokio::task::spawn_blocking(move || extract_blocking(&zip_path, &dst))
        .await
        .map_err(|e| AppError::system(format!("zip extract task join error: {e}")))??;
    tracing::info!(
        op = "zip_extract",
        elapsed_ms = start.elapsed().as_millis(),
        src = %zip_display,
        dst = %dst_display,
        "zip extraction completed"
    );
    Ok(())
}

/// Synchronous extraction for callers that move resource guards into their own blocking worker.
pub fn extract_blocking(zip_path: &Path, dst: &Path) -> AppResult<()> {
    extract_blocking_with_limits(zip_path, dst, EXTRACTION_LIMITS)
}

fn extract_blocking_with_limits(
    zip_path: &Path,
    dst: &Path,
    limits: ExtractionLimits,
) -> AppResult<()> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| AppError::file(format!("zip open failed: {e}")))?;
    extract_open_file_with_limits(file, dst, limits, true, false)
}

/// Extract an already captured snapshot; never reopen its original source path.
pub(crate) fn extract_open_file(file: std::fs::File, dst: &Path) -> AppResult<()> {
    extract_open_file_with_limits(file, dst, EXTRACTION_LIMITS, true, true)
}

/// Validate payload/CRC, limits and link topology with the same extraction loop.
/// Placeholder files preserve topology without writing full payloads twice.
pub(crate) fn validate_open_file(file: std::fs::File) -> AppResult<()> {
    let check = tempfile::tempdir()
        .map_err(|e| AppError::system(format!("create zip validation directory: {e}")))?;
    extract_open_file_with_limits(file, check.path(), EXTRACTION_LIMITS, false, true)
}

fn zip_read_error(error: zip::result::ZipError, context: &str) -> AppError {
    match error {
        zip::result::ZipError::Io(error) => AppError::system(format!("{context}: {error}")),
        other => AppError::file(format!("{context}: {other}")),
    }
}

fn extract_open_file_with_limits(
    file: std::fs::File,
    dst: &Path,
    limits: ExtractionLimits,
    write_payload: bool,
    strict_paths: bool,
) -> AppResult<()> {
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| zip_read_error(e, "zip parse failed"))?;
    if archive.len() > limits.entry_count {
        return Err(AppError::validation(format!(
            "zip contains too many entries (max {})",
            limits.entry_count
        )));
    }
    std::fs::create_dir_all(dst)?;
    let mut extracted_bytes = 0_u64;
    let mut links = shared_types::archive_links::ArchiveSymlinks::default();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| zip_read_error(e, &format!("zip entry {i} read failed")))?;
        let name = entry.name().to_string();
        let target = match safe_zip_entry(dst, &name) {
            Ok(p) => p,
            Err(error) if strict_paths => return Err(error),
            Err(_) => {
                tracing::warn!(entry = %name, "skip unsafe zip entry");
                continue;
            }
        };
        let relative = target
            .strip_prefix(dst)
            .map_err(|e| AppError::validation(format!("archive path: {e}")))?;
        let target = shared_types::archive_links::checked_archive_output(dst, relative)
            .map_err(|e| AppError::validation(format!("archive output: {e}")))?;
        if entry.is_symlink() {
            let mut link = String::new();
            (&mut entry)
                .take((shared_types::archive_links::MAX_ARCHIVE_LINK_BYTES + 1) as u64)
                .read_to_string(&mut link)
                .map_err(payload_error)?;
            let size = link.len() as u64;
            extracted_bytes = extracted_bytes
                .checked_add(size)
                .ok_or_else(|| AppError::validation("zip extracted size overflow"))?;
            if size > limits.file_bytes || extracted_bytes > limits.total_bytes {
                return Err(AppError::validation("zip extracted size exceeds limit"));
            }
            links
                .record(relative, &link)
                .map_err(|e| AppError::validation(format!("archive link: {e}")))?;
            continue;
        }
        if entry.is_dir() {
            std::fs::create_dir_all(&target)?;
            let remaining = limits
                .total_bytes
                .saturating_sub(extracted_bytes)
                .min(limits.file_bytes);
            let copied = std::io::copy(&mut (&mut entry).take(remaining + 1), &mut std::io::sink())
                .map_err(payload_error)?;
            if copied > remaining {
                return Err(AppError::validation("zip directory payload exceeds limit"));
            }
            extracted_bytes += copied;
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
            let copied = if write_payload {
                std::io::copy(&mut (&mut entry).take(copy_limit), &mut out)
            } else {
                std::io::copy(&mut (&mut entry).take(copy_limit), &mut std::io::sink())
            }
            .map_err(payload_error)?;
            if copied >= copy_limit {
                return Err(AppError::validation(format!(
                    "zip entry {name} or extracted total exceeds size limit"
                )));
            }
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                out.set_permissions(std::fs::Permissions::from_mode(mode & 0o777))?;
            }
            extracted_bytes = extracted_bytes
                .checked_add(copied)
                .ok_or_else(|| AppError::validation("zip extracted size overflow"))?;
        }
    }
    links
        .install(dst)
        .map_err(|e| AppError::validation(format!("archive links: {e}")))?;
    Ok(())
}

fn payload_error(error: std::io::Error) -> AppError {
    if error.kind() == std::io::ErrorKind::InvalidData {
        AppError::file(format!("zip payload/CRC is corrupt: {error}"))
    } else {
        AppError::system(format!("read/write zip payload: {error}"))
    }
}

/// Cooperative publishers are locked only during snapshot capture. Even a
/// publisher bypassing that lock cannot replace the opened source object.
pub(crate) fn capture_snapshot(source: &Path, snapshot: &mut std::fs::File) -> AppResult<()> {
    use std::io::{Seek as _, SeekFrom};
    let _guard = acquire_pack_lock(source)?;
    #[cfg(unix)]
    let mut input = std::fs::File::from(
        rustix::fs::open(
            source,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    #[cfg(not(unix))]
    let mut input = std::fs::File::open(source)?;
    let size = input.metadata()?.len();
    // Expanded content already has a 4 GiB budget. Bound the source snapshot
    // too, with an extra GiB for ZIP metadata/headers rather than unlimited I/O.
    if size > MAX_EXTRACTED_TOTAL_BYTES + 1024 * 1024 * 1024 {
        return Err(AppError::validation(
            "restore source exceeds snapshot size limit",
        ));
    }
    if !input.metadata()?.is_file() {
        return Err(AppError::file("restore source is not a regular file"));
    }
    let copy_budget = size
        .checked_add(1)
        .ok_or_else(|| AppError::file("restore source size overflow"))?;
    let copied = std::io::copy(&mut (&mut input).take(copy_budget), snapshot)?;
    if copied != size || input.metadata()?.len() != size {
        return Err(AppError::file(
            "restore source changed during snapshot capture",
        ));
    }
    snapshot.sync_all()?;
    snapshot.seek(SeekFrom::Start(0))?;
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
    /// 显式导出 entry (如 export LATEST 的 cpage_config.json): 打包时源内同名文件
    /// 被跳过, 归档内恰一个该 entry; 真实项目文件/链接不被写入或删除 (FS-02)。
    pub explicit_entry: Option<ExplicitEntry>,
}

/// 归档内显式写入的单个 entry。
#[derive(Clone)]
pub struct ExplicitEntry {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// 同一打包目标的串行守卫: 持有期间从生成临时文件到发布 rename 全程独占。
/// Unix 用目标父目录下的 flock 哨兵文件 (与 storage_contents 的 retirement
/// lock 同模式, 跨进程一致); 非 Unix 退化为进程内全局串行。
pub(crate) struct PackTargetGuard {
    #[cfg(unix)]
    _flock: std::fs::File,
    #[cfg(not(unix))]
    _global: std::sync::MutexGuard<'static, ()>,
}

#[cfg(unix)]
pub(crate) fn acquire_pack_lock(zip_path: &Path) -> AppResult<PackTargetGuard> {
    use rustix::fs::{FlockOperation, flock};
    use sha2::Digest as _;
    let parent = zip_path
        .parent()
        .ok_or_else(|| AppError::file("zip target has no parent directory"))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| AppError::file(format!("create {}: {e}", parent.display())))?;
    let digest = sha2::Sha256::digest(zip_path.as_os_str().as_encoded_bytes());
    // A fixed bucket set bounds persistent lock inodes even for random download
    // targets. Collisions serialize unrelated targets; locks are never unlinked.
    let bucket = digest[0] % 16;
    let lock_path = parent.join(format!(".rcoder-zip-pack-{bucket:02}.lock"));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| AppError::file(format!("open pack lock {}: {e}", lock_path.display())))?;
    flock(&file, FlockOperation::LockExclusive)
        .map_err(|e| AppError::file(format!("acquire pack lock: {e}")))?;
    Ok(PackTargetGuard { _flock: file })
}

#[cfg(not(unix))]
pub(crate) fn acquire_pack_lock(_zip_path: &Path) -> AppResult<PackTargetGuard> {
    static PACK_SERIALIZE: std::sync::LazyLock<std::sync::Mutex<()>> =
        std::sync::LazyLock::new(std::sync::Mutex::default);
    let guard = PACK_SERIALIZE
        .lock()
        .map_err(|_| AppError::system("zip pack serialization mutex is poisoned"))?;
    Ok(PackTargetGuard { _global: guard })
}

/// 在目标同目录完整生成并 finish 后原子发布 (FS-04):
/// 持有同目标串行锁; 遍历/读取/finish 失败只丢弃临时文件, 已存在的有效旧包
/// 保持不变; 发布 rename 成功后旧包才被替换。
fn publish_zip_atomically<F>(zip_path: &Path, write: F) -> AppResult<()>
where
    F: FnOnce(&mut zip::ZipWriter<&std::fs::File>) -> AppResult<()>,
{
    let _guard = acquire_pack_lock(zip_path)?;
    let parent = zip_path
        .parent()
        .ok_or_else(|| AppError::file("zip target has no parent directory"))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| AppError::file(format!("create {}: {e}", parent.display())))?;
    let temp = tempfile::Builder::new()
        .suffix(".pack.tmp")
        .tempfile_in(parent)
        .map_err(|e| AppError::file(format!("create temp zip in {}: {e}", parent.display())))?;
    {
        let mut zip = zip::ZipWriter::new(temp.as_file());
        write(&mut zip)?;
        zip.finish()
            .map_err(|e| AppError::file(format!("zip finish failed: {e}")))?;
    }
    temp.as_file()
        .sync_all()
        .map_err(|e| AppError::file(format!("sync temp zip: {e}")))?;
    temp.persist(zip_path).map_err(|error| {
        AppError::file(format!(
            "publish zip to {}: {}",
            zip_path.display(),
            error.error
        ))
    })?;
    if let Ok(dir) = std::fs::File::open(parent)
        && let Err(error) = dir.sync_all()
    {
        // 发布已完成; 目录 sync 失败不回滚已发布包, 只记录。
        tracing::warn!(error = %error, dir = %parent.display(), "sync zip parent directory failed (published zip intact)");
    }
    Ok(())
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
            explicit_entry: None,
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

pub(crate) fn pack_blocking(src: &Path, zip_path: &Path, opts: &PackOpts) -> AppResult<()> {
    let src = src.to_path_buf();
    let opts = opts.clone();
    publish_zip_atomically(zip_path, move |zip| {
        let has_entries = walk_and_add(&src, &src, zip, &opts)?;
        if !has_entries && let Some(prefix) = opts.path_prefix.as_deref() {
            let entry_name = if prefix.ends_with('/') {
                prefix.to_string()
            } else {
                format!("{prefix}/")
            };
            zip.add_directory(
                &entry_name,
                zip::write::SimpleFileOptions::default().unix_permissions(0o755),
            )
            .map_err(|e| AppError::file(format!("zip add_directory failed: {e}")))?;
        }
        if let Some(explicit) = &opts.explicit_entry {
            zip.start_file(
                &explicit.name,
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .map_err(|e| AppError::file(format!("zip start_file failed: {e}")))?;
            zip.write_all(&explicit.bytes)
                .map_err(|e| AppError::file(format!("zip write explicit entry failed: {e}")))?;
        }
        Ok(())
    })
}

/// 递归遍历并加入 zip (对齐 nuwax archiver.directory 的 entry filter)。
/// 符号链接一律跳过 (lstat); dot-segment/硬链接按 opts; 排除名按 opts。
fn walk_and_add<W: std::io::Write + std::io::Seek>(
    root: &Path,
    dir: &Path,
    zip: &mut zip::ZipWriter<W>,
    opts: &PackOpts,
) -> AppResult<bool> {
    let mut added_entry = false;
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
            let subtree_added = walk_and_add(root, &path, zip, opts)?;
            if !subtree_added {
                let rel = crate::path_safety::host_relative_to_wire(
                    &path
                        .strip_prefix(root)
                        .unwrap_or_else(|_| Path::new(""))
                        .to_string_lossy(),
                );
                let entry_name = match &opts.path_prefix {
                    Some(prefix) => format!("{prefix}{rel}/"),
                    None => format!("{rel}/"),
                };
                zip.add_directory(
                    &entry_name,
                    zip::write::SimpleFileOptions::default().unix_permissions(0o755),
                )
                .map_err(|e| AppError::file(format!("zip add_directory failed: {e}")))?;
            }
            // Child output makes this directory extractable; empty children emit their own entry.
            added_entry = true;
        } else if meta.is_file() {
            if opts.exclude_files.iter().any(|f| f == &name) {
                continue;
            }
            // 硬链接跳过 (nlink>1, 仅 downloadAllFiles)
            if opts.skip_hardlinks && hardlinked(&meta) {
                continue;
            }
            let rel = crate::path_safety::host_relative_to_wire(
                &path
                    .strip_prefix(root)
                    .unwrap_or_else(|_| Path::new(""))
                    .to_string_lossy(),
            );
            // entry 名加 path_prefix (downloadAllFiles 顶层目录前缀)
            let entry_name = match &opts.path_prefix {
                Some(p) => format!("{p}{rel}"),
                None => rel,
            };
            // Replace the exact archive entry, preserving nested files that
            // happen to share its basename.
            if opts
                .explicit_entry
                .as_ref()
                .is_some_and(|explicit| explicit.name == entry_name)
            {
                continue;
            }
            let opts_zip = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file(&entry_name, opts_zip)
                .map_err(|e| AppError::file(format!("zip start_file failed: {e}")))?;
            let mut input = std::fs::File::open(&path)?;
            std::io::copy(&mut input, zip)?;
            added_entry = true;
        }
    }
    Ok(added_entry)
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
    tokio::task::spawn_blocking(move || {
        let name = if dir_entry_name.ends_with('/') {
            dir_entry_name
        } else {
            format!("{dir_entry_name}/")
        };
        publish_zip_atomically(&zip_path, |zip| {
            zip.add_directory(&name, zip::write::SimpleFileOptions::default())
                .map_err(|e| AppError::file(format!("zip add_directory failed: {e}")))?;
            Ok(())
        })
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

    fn entry_content(zip_path: &Path, name: &str) -> Vec<u8> {
        let f = fs::File::open(zip_path).unwrap();
        let mut z = zip::ZipArchive::new(f).unwrap();
        let mut entry = z.by_name(name).unwrap();
        let mut buf = Vec::new();
        Read::read_to_end(&mut entry, &mut buf).unwrap();
        buf
    }

    #[cfg(unix)]
    #[test]
    fn pack_locks_use_a_bounded_number_of_persistent_inodes() {
        let fixture = tempfile::tempdir().unwrap();
        for index in 0..40 {
            drop(acquire_pack_lock(&fixture.path().join(format!("download-{index}.zip"))).unwrap());
        }
        let locks = fs::read_dir(fixture.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".rcoder-zip-pack-")
            })
            .count();
        assert!(
            locks <= 16,
            "random targets leaked {locks} permanent lock inodes"
        );
    }

    #[test]
    fn explicit_entry_preserves_nested_same_basename() {
        let fixture = tempfile::tempdir().unwrap();
        let src = fixture.path().join("src");
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("cpage_config.json"), b"original root").unwrap();
        fs::write(
            src.join("nested/cpage_config.json"),
            b"nested business data",
        )
        .unwrap();
        let target = fixture.path().join("export.zip");
        pack_blocking(
            &src,
            &target,
            &PackOpts {
                explicit_entry: Some(ExplicitEntry {
                    name: "cpage_config.json".into(),
                    bytes: b"export metadata".to_vec(),
                }),
                ..Default::default()
            },
        )
        .unwrap();
        let names = entry_names(&target);
        assert_eq!(
            names
                .iter()
                .filter(|name| *name == "cpage_config.json")
                .count(),
            1
        );
        assert!(
            names.contains(&"nested/cpage_config.json".into()),
            "{names:?}"
        );
        assert_eq!(
            entry_content(&target, "cpage_config.json"),
            b"export metadata"
        );
        assert_eq!(
            entry_content(&target, "nested/cpage_config.json"),
            b"nested business data"
        );
        assert_eq!(
            fs::read(src.join("cpage_config.json")).unwrap(),
            b"original root"
        );
    }

    /// FS-08 反例: POSIX 文件名中的反斜杠是名字的一部分——打包 entry 名必须
    /// 原样保留, 不与真实的 `a/b.txt` 路径发生同名碰撞。修复前无条件替换
    /// `\\ → /` 使两个不同对象变成同名 entry。
    #[cfg(unix)]
    #[test]
    fn pack_preserves_posix_backslash_filenames_without_collision() {
        let fixture_dir = tempfile::tempdir().unwrap();
        let src = fixture_dir.path().join("src");
        fs::create_dir_all(src.join("a")).unwrap();
        fs::write(src.join("a\\b.txt"), b"backslash name").unwrap();
        fs::write(src.join("a/b.txt"), b"slash path").unwrap();
        let target = fixture_dir.path().join("out.zip");
        pack_blocking(&src, &target, &PackOpts::default()).unwrap();

        let names = entry_names(&target);
        assert!(
            names.contains(&"a\\b.txt".to_string()),
            "backslash filename must stay verbatim: {names:?}"
        );
        assert!(
            names.contains(&"a/b.txt".to_string()),
            "real slash path must be untouched: {names:?}"
        );
        assert_eq!(names.len(), 2, "no silent collision: {names:?}");
        assert_eq!(entry_content(&target, "a\\b.txt"), b"backslash name");
        assert_eq!(entry_content(&target, "a/b.txt"), b"slash path");
    }

    /// FS-04 反例: 打包失败（src 不是目录, read_dir 失败发生在目标 create 之后）
    /// 不得截断已存在的有效目标包。修复前 `File::create` 先截断 → 本测试失败。
    #[test]
    fn pack_failure_preserves_existing_target_zip() {
        let fixture_dir = tempfile::tempdir().unwrap();
        let good_src = fixture_dir.path().join("good");
        fs::create_dir_all(&good_src).unwrap();
        fs::write(good_src.join("a.txt"), b"old-backup").unwrap();
        let target = fixture_dir.path().join("backup.zip");
        pack_blocking(&good_src, &target, &PackOpts::default()).unwrap();
        assert_eq!(entry_names(&target), vec!["a.txt".to_string()]);

        let bad_src = fixture_dir.path().join("not-a-dir");
        fs::write(&bad_src, b"plain").unwrap();
        let err = pack_blocking(&bad_src, &target, &PackOpts::default());
        assert!(err.is_err(), "pack must fail when src is not a directory");

        assert_eq!(
            entry_names(&target),
            vec!["a.txt".to_string()],
            "existing backup must stay intact after a failed pack"
        );
        assert_eq!(entry_content(&target, "a.txt"), b"old-backup");
    }

    /// FS-04 反例（遍历中途失败变体）: 源内某文件 open 失败时, 已存在的目标包
    /// 必须保持完整。修复前目标已被截断/半写。
    #[cfg(unix)]
    #[test]
    fn pack_midwalk_open_failure_preserves_existing_target_zip() {
        use std::os::unix::fs::PermissionsExt;
        let fixture_dir = tempfile::tempdir().unwrap();
        let src = fixture_dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("keep.txt"), b"kept").unwrap();
        let target = fixture_dir.path().join("backup.zip");
        pack_blocking(&src, &target, &PackOpts::default()).unwrap();
        assert_eq!(entry_names(&target), vec!["keep.txt".to_string()]);

        let locked = src.join("locked.txt");
        fs::write(&locked, b"secret").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let err = pack_blocking(&src, &target, &PackOpts::default());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(err.is_err(), "pack must fail on unreadable source file");

        assert_eq!(
            entry_names(&target),
            vec!["keep.txt".to_string()],
            "existing backup must stay intact after mid-walk failure"
        );
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
        drop(fs::remove_file(source));
        drop(fs::remove_dir_all(destination));
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
            explicit_entry: None,
        };
        drop(pack_blocking(&src, &out, &opts));
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
        drop(fs::remove_dir_all(&src));
        drop(fs::remove_file(&out));
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
            explicit_entry: None,
        };
        drop(pack_blocking(&src, &out, &opts));
        let names = entry_names(&out);
        assert!(names.contains(&"src/index.js".to_string()), "{names:?}");
        assert!(names.contains(&".gitignore".to_string()), "{names:?}");
        assert!(
            !names.iter().any(|n| n.contains("node_modules")),
            "{names:?}"
        );
        assert!(!names.iter().any(|n| n.contains("dist/")), "{names:?}");
        drop(fs::remove_dir_all(&src));
        drop(fs::remove_file(&out));
    }

    #[test]
    fn packing_preserves_empty_directories_but_omits_implied_parents() {
        let src = unique_tmp("zip_empty_dirs_src");
        let out = unique_tmp("zip_empty_dirs_out");
        fs::create_dir_all(src.join("empty")).expect("create empty directory");
        fs::create_dir_all(src.join("nested")).expect("create nonempty parent");
        fs::write(src.join("nested/file.txt"), "file").expect("write nested file");

        pack_blocking(&src, &out, &PackOpts::default()).expect("pack workspace");
        let names = entry_names(&out);

        assert!(names.contains(&"empty/".to_string()), "{names:?}");
        assert!(names.contains(&"nested/file.txt".to_string()), "{names:?}");
        assert!(!names.contains(&"nested/".to_string()), "{names:?}");

        drop(fs::remove_dir_all(&src));
        drop(fs::remove_file(&out));
    }
}
