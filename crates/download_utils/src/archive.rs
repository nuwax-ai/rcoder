//! Archive extraction utilities
//!
//! Provides safe extraction for tar.gz and zip archives with:
//! - Path traversal protection
//! - Zip bomb protection (size limits)
//! - Directory normalization (strip single wrapper directory)

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use tracing::error;

/// Maximum extracted size (1GB)
const MAX_EXTRACTED_SIZE: u64 = 1024 * 1024 * 1024;

/// Archive extraction error
#[derive(Debug)]
pub enum ArchiveError {
    /// IO error
    Io(std::io::Error),
    /// Path traversal attempt detected
    PathTraversal(String),
    /// Archive bomb detected (too large)
    ArchiveBomb { size: u64, max: u64 },
    /// Invalid archive format
    InvalidArchive(String),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArchiveError::Io(e) => write!(f, "IO error: {}", e),
            ArchiveError::PathTraversal(msg) => write!(f, "Path traversal: {}", msg),
            ArchiveError::ArchiveBomb { size, max } => {
                write!(
                    f,
                    "Archive bomb: {} bytes exceeds limit of {} bytes",
                    size, max
                )
            }
            ArchiveError::InvalidArchive(msg) => write!(f, "Invalid archive: {}", msg),
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
/// Returns the number of file entries extracted.
pub fn extract_tar_gz(archive_path: &Path, dest_dir: &Path) -> Result<usize, ArchiveError> {
    let file = File::open(archive_path)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);

    let mut total_uncompressed: u64 = 0;
    let mut file_count: usize = 0;
    let mut created_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.into_owned();

        let sanitized = sanitize_entry_path(&entry_path)?;
        let dest_path = dest_dir.join(&sanitized);

        // Path safety check
        if let Some(parent) = dest_path.parent()
            && !created_dirs.contains(parent)
        {
            std::fs::create_dir_all(parent)?;
            ensure_within(&dest_path, dest_dir)?;
            created_dirs.insert(parent.to_path_buf());
        }

        let entry_type = entry.header().entry_type();
        let entry_size = entry.header().size()?;

        total_uncompressed = total_uncompressed.saturating_add(entry_size);
        if total_uncompressed > MAX_EXTRACTED_SIZE {
            return Err(ArchiveError::ArchiveBomb {
                size: total_uncompressed,
                max: MAX_EXTRACTED_SIZE,
            });
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
/// Returns the number of file entries extracted.
pub fn extract_zip(archive_path: &Path, dest_dir: &Path) -> Result<usize, ArchiveError> {
    let file = File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| ArchiveError::InvalidArchive(format!("open zip: {e}")))?;

    let mut total_uncompressed: u64 = 0;
    let mut file_count: usize = 0;
    let mut created_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| ArchiveError::InvalidArchive(format!("read zip entry {i}: {e}")))?;
        let entry_name = entry.name().to_string();
        let entry_path = PathBuf::from(&entry_name);

        let sanitized = sanitize_entry_path(&entry_path)?;
        let dest_path = dest_dir.join(&sanitized);

        if let Some(parent) = dest_path.parent()
            && !created_dirs.contains(parent)
        {
            std::fs::create_dir_all(parent)?;
            ensure_within(&dest_path, dest_dir)?;
            created_dirs.insert(parent.to_path_buf());
        }

        let entry_size = entry.size();
        total_uncompressed = total_uncompressed.saturating_add(entry_size);
        if total_uncompressed > MAX_EXTRACTED_SIZE {
            return Err(ArchiveError::ArchiveBomb {
                size: total_uncompressed,
                max: MAX_EXTRACTED_SIZE,
            });
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

/// Detect file type from a file path by reading magic bytes.
pub fn detect_file_type_from_path(path: &Path) -> Result<&'static str, ArchiveError> {
    let mut header = [0u8; 4];
    let mut f = File::open(path)?;
    f.read_exact(&mut header)?;
    Ok(detect_file_type(&header))
}

/// Normalize extracted directory: strip single top-level wrapper.
///
/// If the extraction produced a single top-level directory (e.g. `agent-v1.0.0/`),
/// move its contents up to `agent_dir` directly.
///
/// Returns `true` if a wrapper was stripped, `false` otherwise.
pub fn normalize_extracted_dir(agent_dir: &Path) -> Result<bool, ArchiveError> {
    let entries: Vec<std::fs::DirEntry> = std::fs::read_dir(agent_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            name != ".DS_Store" && !name.to_string_lossy().starts_with("staging.")
        })
        .collect();

    // Check: exactly one directory entry (the wrapper)
    if entries.len() != 1 {
        return Ok(false);
    }
    let only = &entries[0];
    if !only.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
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

    // Move wrapper's children into agent_dir
    let mut moved_names: Vec<std::ffi::OsString> = Vec::new();
    let mut move_err: Option<std::io::Error> = None;
    for entry in std::fs::read_dir(&tmp_rename)? {
        let entry = entry?;
        let name = entry.file_name();
        let dest = agent_dir.join(&name);
        if let Err(e) = std::fs::rename(entry.path(), &dest) {
            move_err = Some(e);
            break;
        }
        moved_names.push(name);
    }

    if let Some(e) = move_err {
        // Rollback
        for name in &moved_names {
            let src = agent_dir.join(name);
            let dst = tmp_rename.join(name);
            if let Err(re) = std::fs::rename(&src, &dst) {
                error!("Rollback failed to move back {}: {}", src.display(), re);
            }
        }
        if let Err(re) = std::fs::rename(&tmp_rename, &wrapper) {
            error!(
                "Rollback failed to restore wrapper dir {}: {}",
                wrapper.display(),
                re
            );
        }
        return Err(ArchiveError::Io(e));
    }

    // Cleanup the now-empty wrapper directory
    let _ = std::fs::remove_dir(&tmp_rename);

    Ok(true)
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
    let content = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;

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

    #[test]
    fn test_sanitize_rejects_parent_dir() {
        assert!(sanitize_entry_path(Path::new("../etc/passwd")).is_err());
    }

    #[test]
    fn test_sanitize_rejects_absolute() {
        assert!(sanitize_entry_path(Path::new("/etc/passwd")).is_err());
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

        let count = extract_tar_gz(&archive, &extract_to).unwrap();
        assert_eq!(count, 1);

        let hello = extract_to.join("bin/hello");
        assert!(hello.exists());
        let content = std::fs::read_to_string(&hello).unwrap();
        assert!(content.contains("echo hi"));
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
    fn test_extract_real_tar_gz() {
        let archive_path = Path::new("/tmp/test-acp-install/cache/test.tar.gz");
        if !archive_path.exists() {
            println!("Skipping test: archive file not found");
            return;
        }

        let tmp_dir = tempdir().unwrap();
        let extract_dir = tmp_dir.path().join("extracted");
        std::fs::create_dir_all(&extract_dir).unwrap();

        // 检测文件类型
        let file_type = detect_file_type_from_path(archive_path).unwrap();
        println!("File type detected: {}", file_type);
        assert_eq!(file_type, "tar.gz");

        // 解压
        let count = extract_tar_gz(archive_path, &extract_dir).unwrap();
        println!("Extracted {} files", count);
        assert!(count > 0);

        // 列出解压后的文件
        println!("\nExtracted files:");
        for entry in std::fs::read_dir(&extract_dir).unwrap() {
            let entry = entry.unwrap();
            println!("  {}", entry.file_name().to_string_lossy());
        }

        // 规范化目录（去掉 wrapper）
        let stripped = normalize_extracted_dir(&extract_dir).unwrap();
        println!("\nStripped wrapper: {}", stripped);
        assert!(stripped);

        // 再次列出文件
        println!("\nFiles after normalization:");
        for entry in std::fs::read_dir(&extract_dir).unwrap() {
            let entry = entry.unwrap();
            println!("  {}", entry.file_name().to_string_lossy());
        }

        // 检查 package.json 存在
        assert!(extract_dir.join("package.json").exists());
    }
}
