//! Archive extraction utilities
//!
//! Provides safe extraction for tar.gz and zip archives with:
//! - Path traversal protection
//! - Directory normalization (strip single wrapper directory)
//! - Optional total-uncompressed-size quota (zip bomb guard)
//!
//! 资源限制(总解压字节)由调用方按需传入 `max_total_size`:
//! - `None` = 不限。解压发生在用户隔离的容器内,由容器/PVC 配额作主边界;用户自己的内容
//!   用爆了,重启容器即可恢复。
//! - `Some(max)` = 累计解压字节超上限即中止。agent 安装包来自外部 URL,须防 zip bomb
//!   (见 agent_runner 侧传 `shared_types::MAX_EXTRACTED_SIZE`)。
//!
//! entry 数与单文件大小上限有意不加,理由同上。path-traversal 安全边界始终生效
//! (防 entry 越出 dest_dir 写到别处)。

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use tracing::{debug, error, warn};

/// Archive extraction error
#[derive(Debug)]
pub enum ArchiveError {
    /// IO error
    Io(std::io::Error),
    /// Path traversal attempt detected
    PathTraversal(String),
    /// Invalid archive format
    InvalidArchive(String),
    /// Cumulative uncompressed size exceeded the caller-supplied quota
    TooLarge { size: u64, max: u64 },
    /// 目录归一化失败且回滚未完全恢复现场: 保留触发回滚的原始错误、回滚中
    /// 失败的步骤(`路径`, 诊断)及可能仍滞留数据的残留路径。
    NormalizeRollbackIncomplete {
        original: Box<ArchiveError>,
        rollback: Vec<(PathBuf, String)>,
        residual: Vec<PathBuf>,
    },
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArchiveError::Io(e) => write!(f, "IO error: {}", e),
            ArchiveError::PathTraversal(msg) => write!(f, "Path traversal: {}", msg),
            ArchiveError::InvalidArchive(msg) => write!(f, "Invalid archive: {}", msg),
            ArchiveError::TooLarge { size, max } => write!(
                f,
                "Archive bomb: uncompressed size {} exceeds limit {}",
                size, max
            ),
            ArchiveError::NormalizeRollbackIncomplete {
                original,
                rollback,
                residual,
            } => {
                let steps = rollback
                    .iter()
                    .map(|(path, msg)| format!("{}: {}", path.display(), msg))
                    .collect::<Vec<_>>()
                    .join("; ");
                let paths = residual
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "normalize rollback incomplete (original error: {original}; failed rollback steps: {steps}; residual paths with possible data: {paths})"
                )
            }
        }
    }
}

impl std::error::Error for ArchiveError {}

impl From<std::io::Error> for ArchiveError {
    fn from(e: std::io::Error) -> Self {
        ArchiveError::Io(e)
    }
}

/// Extract a `.tar.gz` archive into `dest_dir`.
///
/// `max_total_size`: 累计解压字节上限（zip bomb 防护）。`None` = 不限。
///
/// Returns the number of file entries extracted.
pub fn extract_tar_gz(
    archive_path: &Path,
    dest_dir: &Path,
    max_total_size: Option<u64>,
) -> Result<usize, ArchiveError> {
    let file = File::open(archive_path)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);

    let mut file_count: usize = 0;
    let mut total_uncompressed: u64 = 0;
    let mut created_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.into_owned();

        let sanitized = sanitize_entry_path(&entry_path)?;
        let dest_path = dest_dir.join(&sanitized);

        // Path safety check.
        // canonicalize 要求目录已存在, 故先建目录再校验; 校验失败时
        // 尽力回收刚建的空目录, 不在磁盘留下残留。
        if let Some(parent) = dest_path.parent()
            && !created_dirs.contains(parent)
        {
            std::fs::create_dir_all(parent)?;
            if let Err(e) = ensure_within(&dest_path, dest_dir) {
                // 仅删空目录 (非空说明已有内容, 不能删); 失败无害, 记 debug 日志留痕
                if let Err(error) = std::fs::remove_dir(parent) {
                    debug!(
                        path = %parent.display(),
                        %error,
                        "cleanup of directory created for path validation failed"
                    );
                }
                return Err(e);
            }
            created_dirs.insert(parent.to_path_buf());
        }

        let entry_type = entry.header().entry_type();

        // 用 header 声明的大小累计, 在写盘前即可拦下 —— 否则 bomb 会先把磁盘写爆
        if let Some(max) = max_total_size {
            total_uncompressed = total_uncompressed.saturating_add(entry.header().size()?);
            if total_uncompressed > max {
                return Err(ArchiveError::TooLarge {
                    size: total_uncompressed,
                    max,
                });
            }
        }

        if entry_type.is_dir() {
            if !created_dirs.contains(&dest_path) {
                std::fs::create_dir_all(&dest_path)?;
                created_dirs.insert(dest_path);
            }
        } else if entry_type.is_file() {
            file_count += 1;
            let mut out = File::create(&dest_path)?;
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = entry.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
            }

            // 保留归档内文件权限位(如可执行位),不做剥离 —— 容器隔离下由用户自己负责。
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = entry.header().mode()?;
                std::fs::set_permissions(&dest_path, std::fs::Permissions::from_mode(mode))?;
            }
        } else if entry_type.is_symlink() {
            // Skip symlinks for safety
        }
    }

    // Sync once after all files are extracted
    if file_count > 0 {
        let dir = File::open(dest_dir)?;
        dir.sync_all()?;
    }

    Ok(file_count)
}

/// Extract a `.zip` archive into `dest_dir`.
///
/// `max_total_size`: 累计解压字节上限（zip bomb 防护）。`None` = 不限。
///
/// Returns the number of file entries extracted.
pub fn extract_zip(
    archive_path: &Path,
    dest_dir: &Path,
    max_total_size: Option<u64>,
) -> Result<usize, ArchiveError> {
    let file = File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| ArchiveError::InvalidArchive(format!("open zip: {e}")))?;

    let mut file_count: usize = 0;
    let mut total_uncompressed: u64 = 0;
    let mut created_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| ArchiveError::InvalidArchive(format!("read zip entry {i}: {e}")))?;
        let entry_name = entry
            .name()
            .map_err(|e| ArchiveError::InvalidArchive(format!("read zip entry {i} name: {e}")))?
            .into_owned();
        let entry_path = PathBuf::from(&entry_name);

        let sanitized = sanitize_entry_path(&entry_path)?;
        let dest_path = dest_dir.join(&sanitized);

        // 同上: 先建目录再 canonicalize 校验; 校验失败时尽力回收空目录。
        if let Some(parent) = dest_path.parent()
            && !created_dirs.contains(parent)
        {
            std::fs::create_dir_all(parent)?;
            if let Err(e) = ensure_within(&dest_path, dest_dir) {
                // 同 tar.gz 分支: 仅删空目录, 失败无害, 记 debug 日志留痕
                if let Err(error) = std::fs::remove_dir(parent) {
                    debug!(
                        path = %parent.display(),
                        %error,
                        "cleanup of directory created for path validation failed"
                    );
                }
                return Err(e);
            }
            created_dirs.insert(parent.to_path_buf());
        }

        // 用 central directory 声明的未压缩大小累计, 写盘前拦截
        if let Some(max) = max_total_size {
            total_uncompressed = total_uncompressed.saturating_add(entry.size());
            if total_uncompressed > max {
                return Err(ArchiveError::TooLarge {
                    size: total_uncompressed,
                    max,
                });
            }
        }

        if entry.is_dir() {
            if !created_dirs.contains(&dest_path) {
                std::fs::create_dir_all(&dest_path)?;
                created_dirs.insert(dest_path);
            }
        } else if entry.is_file() {
            file_count += 1;
            let mut out = File::create(&dest_path)?;
            std::io::copy(&mut entry, &mut out)?;

            // 保留归档内文件权限位(如可执行位),不做剥离 —— 容器隔离下由用户自己负责。
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dest_path, std::fs::Permissions::from_mode(mode))?;
            }
        }
    }

    // Sync once after all files are extracted
    if file_count > 0 {
        let dir = File::open(dest_dir)?;
        dir.sync_all()?;
    }

    Ok(file_count)
}

/// Detect file type by magic bytes.
///
/// Returns: "tar.gz" | "zip" | "unknown"
pub fn detect_file_type(bytes: &[u8]) -> &'static str {
    if bytes.len() >= 4 {
        // gzip: 1F 8B
        if &bytes[0..2] == b"\x1F\x8B" {
            return "tar.gz";
        }
        // zip: 50 4B 03 04
        if &bytes[0..4] == b"PK\x03\x04" {
            return "zip";
        }
    }
    "unknown"
}

/// Detect file type from a file path by reading up to 4 magic prefix bytes.
///
/// 1~3 字节普通文件按实际前缀分类为 unknown（正常 EOF 不是错误）；空文件保持
/// 既有拒绝策略；真实 IO 错误仍传播。
pub fn detect_file_type_from_path(path: &Path) -> Result<&'static str, ArchiveError> {
    let mut f = File::open(path)?;
    let mut header = Vec::with_capacity(4);
    Read::by_ref(&mut f).take(4).read_to_end(&mut header)?;
    if header.is_empty() {
        return Err(ArchiveError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "file is empty; cannot classify archive type",
        )));
    }
    Ok(detect_file_type(&header))
}

/// Normalize extracted directory: strip single top-level wrapper.
///
/// If the extraction produced a single top-level directory (e.g. `agent-v1.0.0/`),
/// move its contents up to `agent_dir` directly.
///
/// Returns `true` if a wrapper was stripped, `false` otherwise.
///
/// 搬运阶段的**所有**失败出口(子项枚举/迭代/改名)统一先进回滚: 已搬走的子项与
/// wrapper 本体恢复原位后才返回错误; 回滚自身失败时返回
/// [`ArchiveError::NormalizeRollbackIncomplete`], 保留原错误、回滚诊断与可能
/// 滞留数据的残留路径。
pub fn normalize_extracted_dir(agent_dir: &Path) -> Result<bool, ArchiveError> {
    let entries = read_wrapper_candidates(agent_dir)?;

    // Check: exactly one directory entry (the wrapper)
    if entries.len() != 1 {
        return Ok(false);
    }
    let only = &entries[0];
    let file_type = only.file_type().map_err(|e| {
        ArchiveError::Io(std::io::Error::other(format!(
            "stat {}: {e}",
            only.path().display()
        )))
    })?;
    if !file_type.is_dir() {
        return Ok(false);
    }

    let wrapper = only.path();
    let tmp_name = format!(
        "{}__wrapper_tmp",
        agent_dir.file_name().unwrap_or_default().to_string_lossy()
    );
    let tmp_rename = agent_dir.parent().unwrap_or(agent_dir).join(tmp_name);

    // Rename wrapper out of the way
    std::fs::rename(&wrapper, &tmp_rename).map_err(|e| {
        ArchiveError::Io(std::io::Error::other(format!("rename wrapper dir: {}", e)))
    })?;

    // Move wrapper's children into agent_dir; moved 记录已成功搬走的子项供回滚
    let mut moved_names: Vec<std::ffi::OsString> = Vec::new();
    if let Err(original) = move_wrapper_children(&tmp_rename, agent_dir, &mut moved_names) {
        return Err(
            match rollback_wrapper_move(agent_dir, &tmp_rename, &wrapper, &moved_names) {
                Ok(()) => original,
                Err(failure) => ArchiveError::NormalizeRollbackIncomplete {
                    original: Box::new(original),
                    rollback: failure.steps,
                    residual: failure.residual,
                },
            },
        );
    }

    // Cleanup the now-empty wrapper directory; 失败无害 (目录非空或已删), 记 debug 日志留痕
    if let Err(error) = std::fs::remove_dir(&tmp_rename) {
        debug!(
            path = %tmp_rename.display(),
            %error,
            "cleanup of empty wrapper directory failed"
        );
    }

    Ok(true)
}

/// 枚举 agent_dir 下的候选条目(忽略 `.DS_Store`/`staging.*`)。
/// 迭代中的 IO 错误传播而非丢弃——静默丢错会把"无法枚举"误判为"无 wrapper"。
fn read_wrapper_candidates(dir: &Path) -> Result<Vec<std::fs::DirEntry>, ArchiveError> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name != ".DS_Store" && !name.to_string_lossy().starts_with("staging.") {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// 把 wrapper 临时目录的子项逐个搬入 agent_dir; 每次成功改名后登记到 `moved`。
fn move_wrapper_children(
    tmp: &Path,
    agent_dir: &Path,
    moved: &mut Vec<std::ffi::OsString>,
) -> Result<(), ArchiveError> {
    for entry in std::fs::read_dir(tmp)? {
        let entry = entry?;
        let name = entry.file_name();
        std::fs::rename(entry.path(), agent_dir.join(&name))?;
        moved.push(name);
    }
    Ok(())
}

/// 回滚搬运: 已搬入 agent_dir 的子项搬回 wrapper 临时目录, 再把临时目录恢复为
/// wrapper 原名。单个步骤失败不中断其余步骤; 任一失败返回诊断与残留路径。
struct NormalizeRollbackFailure {
    steps: Vec<(PathBuf, String)>,
    residual: Vec<PathBuf>,
}

fn rollback_wrapper_move(
    agent_dir: &Path,
    tmp: &Path,
    wrapper: &Path,
    moved: &[std::ffi::OsString],
) -> Result<(), NormalizeRollbackFailure> {
    let mut steps: Vec<(PathBuf, String)> = Vec::new();
    let mut residual: Vec<PathBuf> = Vec::new();
    for name in moved {
        let src = agent_dir.join(name);
        match std::fs::rename(&src, tmp.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // 子项已不在 agent_dir(无数据可恢复), 只记诊断不列残留
                steps.push((src, format!("move child back: {e}")));
            }
            Err(e) => {
                error!("Rollback failed to move back {}: {}", src.display(), e);
                steps.push((src.clone(), format!("move child back: {e}")));
                residual.push(src);
            }
        }
    }
    match std::fs::rename(tmp, wrapper) {
        Ok(()) => {}
        Err(e) => {
            error!(
                "Rollback failed to restore wrapper dir {}: {}",
                wrapper.display(),
                e
            );
            steps.push((tmp.to_path_buf(), format!("restore wrapper dir: {e}")));
            residual.push(tmp.to_path_buf());
        }
    }
    if steps.is_empty() {
        Ok(())
    } else {
        Err(NormalizeRollbackFailure { steps, residual })
    }
}

/// Locate the entry executable in `extract_dir`.
///
/// Checks:
/// 1. `<extract_dir>/<command>`
/// 2. `<extract_dir>/bin/<command>`
pub fn find_entrypoint(extract_dir: &Path, command: &str) -> Option<PathBuf> {
    let direct = extract_dir.join(command);
    if direct.is_file() {
        return Some(direct);
    }

    let in_bin = extract_dir.join("bin").join(command);
    if in_bin.is_file() {
        return Some(in_bin);
    }

    None
}

/// Read entrypoint from package metadata (`agent-package.json` or `package.json`).
///
/// Returns `(entrypoint_script, extra_args)` if found.
pub fn find_entrypoint_from_metadata(agent_dir: &Path) -> Option<(String, Vec<String>)> {
    // Try agent-package.json first
    let agent_pkg = agent_dir.join("agent-package.json");
    if let Some(result) = read_bin_start_from_json(&agent_pkg) {
        return Some(result);
    }

    // Fallback to package.json
    let pkg = agent_dir.join("package.json");
    if let Some(result) = read_bin_start_from_json(&pkg) {
        return Some(result);
    }

    None
}

/// Read `bin.start` from a JSON file.
fn read_bin_start_from_json(path: &Path) -> Option<(String, Vec<String>)> {
    // 区分"文件不存在"（正常，试下一个候选）与"存在但读取/解析失败"（配置 bug，须可见）。
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!("failed to read package json {}: {e}", path.display());
            return None;
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            warn!("failed to parse package json {}: {e}", path.display());
            return None;
        }
    };

    let bin_start = value.get("bin")?.get("start")?.as_str()?;

    let parts: Vec<&str> = bin_start.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }

    let runtimes = ["node", "deno", "bun", "python", "python3"];
    if parts.len() > 1 && runtimes.contains(&parts[0]) {
        let rest = &parts[1..];
        let mut flags: Vec<String> = Vec::new();
        let mut script: Option<String> = None;
        let mut extra_args: Vec<String> = Vec::new();

        for part in rest {
            if script.is_none() {
                if part.starts_with('-') {
                    flags.push(part.to_string());
                } else {
                    script = Some(part.to_string());
                }
            } else {
                extra_args.push(part.to_string());
            }
        }

        let script = script?;
        let mut args = flags;
        args.extend(extra_args);
        Some((script, args))
    } else {
        let script = parts[0].to_string();
        let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
        Some((script, args))
    }
}

fn sanitize_entry_path(path: &Path) -> Result<PathBuf, ArchiveError> {
    let path_str = path.to_string_lossy();
    if path_str.contains('\0') {
        return Err(ArchiveError::PathTraversal(format!(
            "NUL byte in entry path: {path_str}"
        )));
    }
    for component in path.components() {
        match component {
            Component::ParentDir => {
                return Err(ArchiveError::PathTraversal(format!(
                    "parent dir component in entry: {path_str}"
                )));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(ArchiveError::PathTraversal(format!(
                    "absolute path in entry: {path_str}"
                )));
            }
            Component::Normal(_) | Component::CurDir => {}
        }
    }
    Ok(path.to_path_buf())
}

fn ensure_within(dest_path: &Path, base: &Path) -> Result<(), ArchiveError> {
    // Fail-closed: canonicalize 失败时拒绝而非回退到未验证路径。
    // 防御符号链接攻击: canonicalize 解析符号链接后验证真实路径。
    let base_canon = base.canonicalize().map_err(|e| {
        ArchiveError::PathTraversal(format!(
            "cannot canonicalize base dir {}: {}",
            base.display(),
            e
        ))
    })?;

    let parent = dest_path.parent().unwrap_or(dest_path);
    let canon_parent = parent.canonicalize().map_err(|e| {
        ArchiveError::PathTraversal(format!(
            "cannot canonicalize parent dir {}: {}",
            parent.display(),
            e
        ))
    })?;

    if !canon_parent.starts_with(&base_canon) {
        return Err(ArchiveError::PathTraversal(format!(
            "entry escapes dest dir: dest={}, base={}",
            canon_parent.display(),
            base_canon.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_detect_tar_gz() {
        assert_eq!(detect_file_type(b"\x1F\x8B\x08\x00"), "tar.gz");
    }

    #[test]
    fn test_detect_zip() {
        assert_eq!(detect_file_type(b"PK\x03\x04\x14\x00"), "zip");
    }

    #[test]
    fn test_detect_unknown() {
        assert_eq!(detect_file_type(b"\x7FELF\x02\x01"), "unknown");
    }

    /// R08 反例: read_exact 假定填满 4 字节, 1~3 字节普通文件会因 UnexpectedEof
    /// 被整体拒绝。修复后正常 EOF 按实际前缀分类为 unknown; 空文件策略保持拒绝;
    /// 真实 IO 错误(文件不存在)仍传播。
    #[test]
    fn detect_file_type_from_path_reads_short_prefixes() {
        let tmp = tempdir().unwrap();
        for len in 1..=3 {
            let path = tmp.path().join(format!("short_{len}"));
            std::fs::write(&path, vec![b'x'; len]).unwrap();
            assert_eq!(
                detect_file_type_from_path(&path).unwrap(),
                "unknown",
                "{len}-byte plain file must classify as unknown, not fail"
            );
        }
        // 4 字节仍走完整魔数识别
        let path = tmp.path().join("zip4");
        std::fs::write(&path, b"PK\x03\x04").unwrap();
        assert_eq!(detect_file_type_from_path(&path).unwrap(), "zip");
        let path = tmp.path().join("gz4");
        std::fs::write(&path, b"\x1F\x8B\x08\x00").unwrap();
        assert_eq!(detect_file_type_from_path(&path).unwrap(), "tar.gz");

        // 空文件策略保持现状(拒绝), 不在 R08 中改变
        let path = tmp.path().join("empty");
        std::fs::write(&path, b"").unwrap();
        assert!(
            detect_file_type_from_path(&path).is_err(),
            "empty-file policy must stay rejected"
        );
        // 文件不存在等真实 IO 错误仍传播
        assert!(detect_file_type_from_path(&tmp.path().join("missing")).is_err());
    }

    #[test]
    fn test_sanitize_rejects_parent_dir() {
        assert!(sanitize_entry_path(Path::new("../etc/passwd")).is_err());
    }

    #[test]
    fn test_sanitize_rejects_absolute() {
        assert!(sanitize_entry_path(Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn test_sanitize_rejects_nul() {
        assert!(sanitize_entry_path(Path::new("foo\0bar")).is_err());
    }

    #[test]
    fn test_extract_tar_gz_round_trip() {
        let tmp = tempdir().unwrap();
        let archive = tmp.path().join("src.tar.gz");
        let extract_to = tmp.path().join("out");
        std::fs::create_dir_all(&extract_to).unwrap();

        {
            let tar_file = File::create(&archive).unwrap();
            let enc = flate2::write::GzEncoder::new(tar_file, flate2::Compression::fast());
            let mut tar = tar::Builder::new(enc);

            let mut header = tar::Header::new_gnu();
            let data = b"#!/bin/sh\necho hi\n";
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, "bin/hello", &data[..])
                .unwrap();

            tar.into_inner().unwrap().finish().unwrap();
        }

        let count = extract_tar_gz(&archive, &extract_to, None).unwrap();
        assert_eq!(count, 1);

        let hello = extract_to.join("bin/hello");
        assert!(hello.exists());
        let content = std::fs::read_to_string(&hello).unwrap();
        assert!(content.contains("echo hi"));
    }

    #[test]
    fn extract_tar_preserves_file_permissions() {
        // 资源/权限限制已移除(容器隔离):归档内的权限位原样保留(含可执行位),不做剥离。
        let tmp = tempdir().unwrap();
        let archive = tmp.path().join("perms.tar.gz");
        let extract_to = tmp.path().join("out");
        std::fs::create_dir_all(&extract_to).unwrap();

        {
            let tar_file = File::create(&archive).unwrap();
            let enc = flate2::write::GzEncoder::new(tar_file, flate2::Compression::fast());
            let mut tar = tar::Builder::new(enc);
            let mut header = tar::Header::new_gnu();
            let data = b"#!/bin/sh\necho hi\n";
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, "hello", &data[..]).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }

        assert_eq!(extract_tar_gz(&archive, &extract_to, None).unwrap(), 1);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(extract_to.join("hello"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "executable bits must be preserved");
        }
    }

    #[test]
    fn test_normalize_strips_wrapper() {
        let tmp = tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();

        // Create wrapper directory
        let wrapper = agent_dir.join("wrapper-1.0.0");
        std::fs::create_dir_all(&wrapper).unwrap();
        std::fs::write(wrapper.join("file.txt"), "content").unwrap();

        let stripped = normalize_extracted_dir(&agent_dir).unwrap();
        assert!(stripped);
        assert!(agent_dir.join("file.txt").exists());
        assert!(!wrapper.exists());
    }

    #[test]
    fn test_normalize_no_wrapper() {
        let tmp = tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();

        // Create multiple entries (no wrapper)
        std::fs::write(agent_dir.join("file1.txt"), "content1").unwrap();
        std::fs::write(agent_dir.join("file2.txt"), "content2").unwrap();

        let stripped = normalize_extracted_dir(&agent_dir).unwrap();
        assert!(!stripped);
    }

    /// R07 反例: wrapper 已移出 agent_dir 后, 子项枚举失败(read_dir(tmp) 的 `?`)
    /// 直接返回错误、跳过回滚——wrapper 被滞留在目标目录外。用仅写(0o200)权限的
    /// wrapper 确定性注入: rename 出去只需写权限(macOS/Linux 均如此), 而后续
    /// read_dir(tmp) 需要读权限而失败; 初始 read_dir(agent_dir) 与 DirEntry 的
    /// file_type 只依赖 agent_dir 自身权限, 不受影响。修复后必须先回滚再返回错误。
    #[cfg(unix)]
    #[test]
    fn normalize_children_enumeration_failure_restores_wrapper() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let wrapper = agent_dir.join("wrapper-1.0.0");
        std::fs::create_dir_all(&wrapper).unwrap();
        std::fs::write(wrapper.join("file.txt"), "content").unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o200)).unwrap();

        let result = normalize_extracted_dir(&agent_dir);

        // 测试卫生: 无论断言结果如何, 先恢复两处可能位置的权限便于清理
        let stranded = tmp.path().join("agent__wrapper_tmp");
        if stranded.is_dir() {
            drop(std::fs::set_permissions(
                &stranded,
                std::fs::Permissions::from_mode(0o755),
            ));
        }
        drop(std::fs::set_permissions(
            &wrapper,
            std::fs::Permissions::from_mode(0o755),
        ));

        assert!(result.is_err(), "unreadable wrapper must surface an error");
        assert!(
            wrapper.is_dir(),
            "wrapper must be rolled back to {} instead of stranded at {}",
            wrapper.display(),
            stranded.display()
        );
        assert!(!stranded.exists(), "no stray wrapper temp dir may remain");
        assert_eq!(
            std::fs::read_to_string(wrapper.join("file.txt")).unwrap(),
            "content",
            "rolled-back wrapper must keep its contents"
        );
    }

    /// R07: 子项搬移中途冲突失败时, 原树必须完整恢复(现有回滚分支的守护)。
    /// agent_dir 中被过滤的 `.DS_Store` 是非空目录, 与 wrapper 内同名文件冲突。
    #[test]
    fn normalize_mid_move_conflict_restores_original_tree() {
        let tmp = tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(agent_dir.join(".DS_Store").join("inner")).unwrap();
        std::fs::write(
            agent_dir.join(".DS_Store").join("inner").join("keep.txt"),
            "x",
        )
        .unwrap();
        let wrapper = agent_dir.join("wrapper-1.0.0");
        std::fs::create_dir_all(&wrapper).unwrap();
        // wrapper 子项: 同名文件(搬移必然冲突) + 正常文件(可能已被搬走)
        std::fs::write(wrapper.join(".DS_Store"), "wrapper mac file").unwrap();
        std::fs::write(wrapper.join("real.txt"), "real").unwrap();

        let result = normalize_extracted_dir(&agent_dir);

        assert!(result.is_err(), "conflicting child move must fail");
        assert!(
            wrapper.join(".DS_Store").is_file(),
            "wrapper must be restored with all children"
        );
        assert_eq!(
            std::fs::read_to_string(wrapper.join("real.txt")).unwrap(),
            "real"
        );
        assert!(
            agent_dir
                .join(".DS_Store")
                .join("inner")
                .join("keep.txt")
                .is_file()
        );
        assert!(
            !tmp.path().join("agent__wrapper_tmp").exists(),
            "no stray wrapper temp dir may remain"
        );
    }

    /// R07: 初始枚举失败(目录不存在)原样传播, 无副作用。
    #[test]
    fn normalize_missing_dir_error_propagates_without_side_effects() {
        let tmp = tempdir().unwrap();
        assert!(normalize_extracted_dir(&tmp.path().join("nope")).is_err());
    }

    /// R07: 回滚中个别子项已不可恢复(如被外部移走)时, 其余步骤继续、wrapper
    /// 仍恢复, 失败步骤与残留路径进入诊断。直接在手工构造的"搬入一半"现场上
    /// 调用回滚(该状态无法用纯文件系统权限在完整链路中确定性注入)。
    #[test]
    fn rollback_reports_unrecoverable_steps_and_residuals() {
        let tmp = tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        let tmp_wrapper = tmp.path().join("agent__wrapper_tmp");
        let wrapper = agent_dir.join("wrapper-1.0.0");
        for dir in [&agent_dir, &tmp_wrapper] {
            std::fs::create_dir_all(dir).unwrap();
        }
        // 模拟: a.txt 已搬入 agent_dir; b.txt 在 moved 清单里但已被外部移走
        std::fs::write(agent_dir.join("a.txt"), "a").unwrap();
        std::fs::write(tmp_wrapper.join("b.txt"), "b").unwrap();
        let moved = vec!["a.txt".into(), "b.txt".into()];

        let failure =
            rollback_wrapper_move(&agent_dir, &tmp_wrapper, &wrapper, &moved).unwrap_err();

        // 可恢复子项搬回 + wrapper 恢复原名
        assert!(wrapper.join("a.txt").is_file());
        assert!(wrapper.join("b.txt").is_file());
        assert!(!tmp_wrapper.exists());
        // 不可恢复的 b.txt 记为失败步骤; ENOENT 无数据残留, 不列入 residual
        assert_eq!(failure.steps.len(), 1, "{:?}", failure.steps);
        assert_eq!(failure.steps[0].0, agent_dir.join("b.txt"));
        assert!(
            failure.residual.is_empty(),
            "ENOENT steps carry no data, residual={:?}",
            failure.residual
        );
    }

    /// R07: 错误显示保留原错误与残留路径(现场可定位)。
    #[test]
    fn rollback_incomplete_error_display_carries_diagnostics() {
        let error = ArchiveError::NormalizeRollbackIncomplete {
            original: Box::new(ArchiveError::Io(std::io::Error::other("boom"))),
            rollback: vec![(
                PathBuf::from("/tmp/agent__wrapper_tmp"),
                "restore wrapper dir: permission denied".to_string(),
            )],
            residual: vec![PathBuf::from("/tmp/agent__wrapper_tmp")],
        };
        let text = error.to_string();
        assert!(text.contains("boom"), "{text}");
        assert!(text.contains("restore wrapper dir"), "{text}");
        assert!(text.contains("agent__wrapper_tmp"), "{text}");
    }

    #[test]
    fn test_find_entrypoint() {
        let tmp = tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();

        // No entrypoint
        assert!(find_entrypoint(&agent_dir, "myapp").is_none());

        // Direct entrypoint
        std::fs::write(agent_dir.join("myapp"), "#!/bin/sh\necho hi").unwrap();
        assert!(find_entrypoint(&agent_dir, "myapp").is_some());

        // Bin entrypoint
        std::fs::remove_file(agent_dir.join("myapp")).unwrap();
        std::fs::create_dir_all(agent_dir.join("bin")).unwrap();
        std::fs::write(agent_dir.join("bin").join("myapp"), "#!/bin/sh\necho hi").unwrap();
        assert!(find_entrypoint(&agent_dir, "myapp").is_some());
    }

    #[test]
    fn test_extract_generated_tar_gz() {
        // 测试内动态构建 tar.gz（不再依赖 /tmp 外部文件，避免静默 skip）：
        // 内容为带 wrapper 目录的结构 testpkg/{package.json, index.js}
        let tmp = tempdir().unwrap();

        // 1. 准备源目录
        let wrapper_dir = tmp.path().join("testpkg");
        std::fs::create_dir_all(&wrapper_dir).unwrap();
        std::fs::write(wrapper_dir.join("package.json"), r#"{"name":"testpkg"}"#).unwrap();
        std::fs::write(wrapper_dir.join("index.js"), "console.log('hi');").unwrap();

        // 2. 打包为 tar.gz（wrapper 目录作为归档顶层条目）
        let archive_path = tmp.path().join("test.tar.gz");
        {
            let file = File::create(&archive_path).unwrap();
            let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut builder = tar::Builder::new(enc);
            builder.append_dir_all("testpkg", &wrapper_dir).unwrap();
            builder.finish().unwrap();
        }

        // 3. 检测文件类型
        let file_type = detect_file_type_from_path(&archive_path).unwrap();
        assert_eq!(file_type, "tar.gz");

        // 4. 解压
        let extract_dir = tmp.path().join("extracted");
        std::fs::create_dir_all(&extract_dir).unwrap();
        let count = extract_tar_gz(&archive_path, &extract_dir, None).unwrap();
        assert!(count > 0);

        // 5. 规范化目录（去掉 wrapper）
        let stripped = normalize_extracted_dir(&extract_dir).unwrap();
        assert!(stripped);

        // 6. 检查内容直接位于解压根目录
        assert!(extract_dir.join("package.json").exists());
        assert!(extract_dir.join("index.js").exists());
    }

    /// 手写一个 tar 归档字节流(单 file entry,GNU long path 不启用)
    ///
    /// tar crate 的 `set_path` 自身会拒绝 `..` 路径，故手写 tar 头绕过 Builder
    /// 校验，才能验证**我们的** extract 逻辑确实拦得下。
    fn build_tar_with_path(name: &str, data: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        let name_bytes = name.as_bytes();
        let copy_len = name_bytes.len().min(100);
        header[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

        header[100..108].copy_from_slice(b"0000644\0"); // mode
        header[108..116].copy_from_slice(b"0000000\0"); // uid
        header[116..124].copy_from_slice(b"0000000\0"); // gid
        let size_str = format!("{:011o}\0", data.len());
        header[124..136].copy_from_slice(size_str.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0"); // mtime
        header[156] = b'0'; // typeflag: regular file
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");

        // checksum 字段本身按 spaces 参与计算
        let mut chs = [b' '; 8];
        header[148..156].copy_from_slice(&chs);
        let sum: u32 = header.iter().map(|&b| b as u32).sum();
        let cksum = format!("{:06o}\0 ", sum);
        chs.copy_from_slice(cksum.as_bytes());
        header[148..156].copy_from_slice(&chs);

        let mut out = Vec::new();
        out.extend_from_slice(&header);
        out.extend_from_slice(data);
        let pad = (512 - (data.len() % 512)) % 512;
        out.extend(std::iter::repeat_n(0u8, pad));
        out.extend(std::iter::repeat_n(0u8, 1024)); // 2 个零块作 EOF
        out
    }

    fn write_tar_gz(path: &Path, raw_tar: &[u8]) {
        let mut gz =
            flate2::write::GzEncoder::new(File::create(path).unwrap(), flate2::Compression::fast());
        Write::write_all(&mut gz, raw_tar).unwrap();
        gz.finish().unwrap();
    }

    #[test]
    fn extract_tar_gz_rejects_path_traversal() {
        let tmp = tempdir().unwrap();
        let archive = tmp.path().join("evil.tar.gz");
        let extract_to = tmp.path().join("out");
        std::fs::create_dir_all(&extract_to).unwrap();

        write_tar_gz(
            &archive,
            &build_tar_with_path("../../../etc/evil", b"pwned"),
        );

        let err = extract_tar_gz(&archive, &extract_to, None).unwrap_err();
        assert!(matches!(err, ArchiveError::PathTraversal(_)));
        // 越界文件绝不能落盘
        assert!(!Path::new("/etc/evil").exists());
        assert!(!extract_to.join("evil").exists());
    }

    #[test]
    fn extract_zip_rejects_path_traversal() {
        let tmp = tempdir().unwrap();
        let archive = tmp.path().join("evil.zip");
        let extract_to = tmp.path().join("out");
        std::fs::create_dir_all(&extract_to).unwrap();

        {
            let file = File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts: zip::write::SimpleFileOptions = zip::write::FileOptions::default();
            zip.start_file("../../../etc/evil", opts).unwrap();
            zip.write_all(b"pwned").unwrap();
            zip.finish().unwrap();
        }

        let err = extract_zip(&archive, &extract_to, None).unwrap_err();
        assert!(matches!(err, ArchiveError::PathTraversal(_)));
        assert!(!extract_to.join("evil").exists());
    }

    /// 配额检查此前两侧实现都无测试覆盖。用"小配额 + 正常条目"确定性触发，
    /// 比伪造超大 tar header 可靠（后者会先撞上 header 与实际数据不符的解析错）。
    #[test]
    fn extract_tar_gz_enforces_quota_when_some() {
        let tmp = tempdir().unwrap();
        let archive = tmp.path().join("src.tar.gz");
        let extract_to = tmp.path().join("out");
        std::fs::create_dir_all(&extract_to).unwrap();

        let payload = vec![b'x'; 4096];
        write_tar_gz(&archive, &build_tar_with_path("big.bin", &payload));

        // 配额远小于条目声明大小 → 必须在写盘前拦下
        let err = extract_tar_gz(&archive, &extract_to, Some(16)).unwrap_err();
        match err {
            ArchiveError::TooLarge { size, max } => {
                assert_eq!(max, 16);
                assert!(size >= 4096, "size={size}");
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn extract_zip_enforces_quota_when_some() {
        let tmp = tempdir().unwrap();
        let archive = tmp.path().join("src.zip");
        let extract_to = tmp.path().join("out");
        std::fs::create_dir_all(&extract_to).unwrap();

        {
            let file = File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts: zip::write::SimpleFileOptions = zip::write::FileOptions::default();
            zip.start_file("big.bin", opts).unwrap();
            zip.write_all(&vec![b'y'; 4096]).unwrap();
            zip.finish().unwrap();
        }

        let err = extract_zip(&archive, &extract_to, Some(16)).unwrap_err();
        assert!(matches!(err, ArchiveError::TooLarge { max: 16, .. }));
    }

    /// 同一个归档在 `None`（不限额）下必须正常解压——证明配额是可选的、
    /// 不会改变既有调用方（agent_provisioning / file-server-userapp）的行为。
    #[test]
    fn quota_none_does_not_limit() {
        let tmp = tempdir().unwrap();
        let archive = tmp.path().join("src.tar.gz");
        let extract_to = tmp.path().join("out");
        std::fs::create_dir_all(&extract_to).unwrap();

        let payload = vec![b'x'; 4096];
        write_tar_gz(&archive, &build_tar_with_path("big.bin", &payload));

        let count = extract_tar_gz(&archive, &extract_to, None).unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            std::fs::metadata(extract_to.join("big.bin")).unwrap().len(),
            4096
        );
    }
}
