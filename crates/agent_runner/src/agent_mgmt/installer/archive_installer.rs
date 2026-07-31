//! Archive extraction with safety guarantees (P0-1)
//!
//! Two formats supported: `tar.gz` and `zip`. Both are guarded against:
//! - **Path traversal**: entry names that escape the destination (e.g. `../../etc/passwd`,
//!   absolute paths, Windows drive letters, NUL bytes)
//! - **Zip bomb**: cumulative uncompressed size exceeding [`shared_types::MAX_EXTRACTED_SIZE`]
//!
//! Only tar.gz and zip archives are accepted; non-archive files are rejected
//! upstream in [`binary_installer`] with `UnsupportedType`.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use shared_types::MAX_EXTRACTED_SIZE;
use tracing::{debug, warn};

use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};

/// Extract a `.tar.gz` archive into `dest_dir`.
///
/// Validates every entry:
/// 1. No `..` components, no absolute paths, no NUL bytes
/// 2. Cumulative uncompressed size <= [`MAX_EXTRACTED_SIZE`]
///
/// Returns the number of file entries extracted.
pub fn extract_tar_gz(archive_path: &Path, dest_dir: &Path) -> AgentMgmtResult<usize> {
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

        // 路径安全检查：只对首次出现的目录做 canonicalize
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
            return Err(AgentMgmtError::ArchiveBomb {
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
            // 不逐文件 sync_all() — 解压完成后会对目标目录整体 sync 一次

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = entry.header().mode()?;
                std::fs::set_permissions(&dest_path, std::fs::Permissions::from_mode(mode))?;
            }
        } else if entry_type.is_symlink() {
            warn!(
                "[agent_mgmt] Skipping symlink entry in tar.gz: {}",
                entry_path.display()
            );
        } else {
            debug!(
                "[agent_mgmt] Skipping unsupported tar entry type: {:?} ({})",
                entry_type,
                entry_path.display()
            );
        }
    }

    // 整体 sync 一次：保证所有文件数据落盘后再返回
    // 避免进程被 kill 后目标目录中出现不完整的文件
    if file_count > 0 {
        let dir = File::open(dest_dir)?;
        dir.sync_all()?;
    }

    Ok(file_count)
}

/// Extract a `.zip` archive into `dest_dir`.
///
/// Returns the number of file entries extracted.
pub fn extract_zip(archive_path: &Path, dest_dir: &Path) -> AgentMgmtResult<usize> {
    let file = File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| AgentMgmtError::Archive(format!("open zip: {e}")))?;

    let mut total_uncompressed: u64 = 0;
    let mut file_count: usize = 0;
    let mut created_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| AgentMgmtError::Archive(format!("read zip entry {i}: {e}")))?;
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
            return Err(AgentMgmtError::ArchiveBomb {
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
            // 不逐文件 sync_all() — 解压完成后会对目标目录整体 sync 一次

            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dest_path, std::fs::Permissions::from_mode(mode))?;
            }
        } else {
            debug!(
                "[agent_mgmt] Skipping unsupported zip entry: {}",
                entry_name
            );
        }
    }

    // 整体 sync 一次：保证所有文件数据落盘后再返回
    if file_count > 0 {
        let dir = File::open(dest_dir)?;
        dir.sync_all()?;
    }

    Ok(file_count)
}

/// Locate the entry executable in `extract_dir`.
///
/// Checks exact paths only (no ambiguous fallback):
/// 1. `<extract_dir>/<command>`
/// 2. `<extract_dir>/bin/<command>`
///
/// Note: uses `is_file()` instead of `is_executable_file()` to support
/// non-binary entrypoints (e.g. Node.js scripts without +x permission).
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

/// Normalize extracted directory: strip single top-level wrapper.
///
/// If the extraction produced a single top-level directory (e.g. `deepagents-dev-templates-0.2.9/`),
/// move its contents up to `agent_dir` directly. This ensures the installed layout
/// matches the expected structure without an extra nesting level.
///
/// Returns `true` if a wrapper was stripped, `false` otherwise.
pub fn normalize_extracted_dir(agent_dir: &Path) -> AgentMgmtResult<bool> {
    let entries: Vec<std::fs::DirEntry> = std::fs::read_dir(agent_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            name != ".DS_Store" && name != "staging.tar.gz" && name != "staging.zip"
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
        AgentMgmtError::InstallFailed(format!(
            "rename wrapper dir: {} -> {}: {}",
            wrapper.display(),
            tmp_rename.display(),
            e
        ))
    })?;

    // Move wrapper's children into agent_dir, tracking moved entries for rollback
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
        // Rollback: only move back the entries that were successfully moved
        for name in &moved_names {
            let src = agent_dir.join(name);
            let dst = tmp_rename.join(name);
            if let Err(re) = std::fs::rename(&src, &dst) {
                warn!(
                    "[agent_mgmt] Rollback failed to move back {}: {}",
                    src.display(),
                    re
                );
            }
        }
        if let Err(re) = std::fs::rename(&tmp_rename, &wrapper) {
            warn!(
                "[agent_mgmt] Rollback failed to restore wrapper dir {}: {}",
                wrapper.display(),
                re
            );
        }
        return Err(AgentMgmtError::InstallFailed(format!(
            "move wrapper child: {}",
            e
        )));
    }

    // Cleanup the now-empty wrapper directory
    let _ = std::fs::remove_dir(&tmp_rename);

    debug!(
        "[agent_mgmt] Stripped top-level wrapper directory: {}",
        wrapper.display()
    );
    Ok(true)
}

/// Read entrypoint from package metadata (`agent-package.json` or `package.json`).
///
/// Returns `(entrypoint_script, extra_args)` if found, e.g. `("dist/index.js", [])`.
/// Checks `agent-package.json` first, then falls back to `package.json`.
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

/// Read `bin.start` from a JSON file and split into (script, extra_args).
///
/// Supported formats:
/// - `"dist/index.js"` → `("dist/index.js", [])`
/// - `"node dist/index.js"` → `("dist/index.js", [])`
/// - `"node --max-old-space-size=4096 dist/index.js"` → `("dist/index.js", ["--max-old-space-size=4096"])`
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
        // Skip runtime, collect flags (starting with -), then find the script path
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

        // Return script with flags prepended to args
        let script = script?;
        let mut args = flags;
        args.extend(extra_args);
        Some((script, args))
    } else {
        // Single entrypoint like "dist/index.js"
        let script = parts[0].to_string();
        let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
        Some((script, args))
    }
}

#[allow(dead_code)]
fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            return meta.permissions().mode() & 0o111 != 0;
        }
        false
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

fn sanitize_entry_path(path: &Path) -> AgentMgmtResult<PathBuf> {
    let path_str = path.to_string_lossy();
    if path_str.contains('\0') {
        return Err(AgentMgmtError::PathTraversal(format!(
            "NUL byte in entry path: {path_str}"
        )));
    }
    for component in path.components() {
        match component {
            Component::ParentDir => {
                return Err(AgentMgmtError::PathTraversal(format!(
                    "parent dir component in entry: {path_str}"
                )));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(AgentMgmtError::PathTraversal(format!(
                    "absolute path in entry: {path_str}"
                )));
            }
            Component::Normal(_) | Component::CurDir => {}
        }
    }
    Ok(path.to_path_buf())
}

fn ensure_within(dest_path: &Path, base: &Path) -> AgentMgmtResult<()> {
    // Fail-closed: canonicalize 失败时拒绝而非回退到未验证路径。
    // 防御符号链接攻击: canonicalize 解析符号链接后验证真实路径。
    let base_canon = base.canonicalize().map_err(|e| {
        AgentMgmtError::PathTraversal(format!(
            "cannot canonicalize base dir {}: {}",
            base.display(),
            e
        ))
    })?;

    let parent = dest_path.parent().unwrap_or(dest_path);
    let canon_parent = parent.canonicalize().map_err(|e| {
        AgentMgmtError::PathTraversal(format!(
            "cannot canonicalize parent dir {}: {}",
            parent.display(),
            e
        ))
    })?;

    if !canon_parent.starts_with(&base_canon) {
        return Err(AgentMgmtError::PathTraversal(format!(
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
    fn sanitize_rejects_parent_dir() {
        assert!(sanitize_entry_path(Path::new("../etc/passwd")).is_err());
        assert!(sanitize_entry_path(Path::new("foo/../../bar")).is_err());
    }

    #[test]
    fn sanitize_rejects_absolute() {
        #[cfg(unix)]
        assert!(sanitize_entry_path(Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn sanitize_rejects_nul() {
        assert!(sanitize_entry_path(Path::new("foo\0bar")).is_err());
    }

    #[test]
    fn extract_tar_gz_round_trip() {
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

            let mut header2 = tar::Header::new_gnu();
            let data2 = b"hello world";
            header2.set_size(data2.len() as u64);
            header2.set_mode(0o644);
            header2.set_cksum();
            tar.append_data(&mut header2, "lib/data.txt", &data2[..])
                .unwrap();

            tar.into_inner().unwrap().finish().unwrap();
        }

        extract_tar_gz(&archive, &extract_to).unwrap();
        let hello = extract_to.join("bin/hello");
        assert!(hello.exists());
        let content = std::fs::read_to_string(&hello).unwrap();
        assert!(content.contains("echo hi"));
    }

    #[test]
    fn extract_tar_gz_rejects_path_traversal() {
        // tar crate 的 set_path 自身会拒绝 ".." 路径,因此我们手写一个 tar 头
        // 来绕过 Builder 验证,确保我们的 extract 逻辑能拦下。
        let tmp = tempdir().unwrap();
        let archive = tmp.path().join("evil.tar.gz");
        let extract_to = tmp.path().join("out");
        std::fs::create_dir_all(&extract_to).unwrap();

        let raw_tar = build_tar_with_path("../../../etc/evil", b"pwned");
        // gzip compress
        let mut gz = flate2::write::GzEncoder::new(
            File::create(&archive).unwrap(),
            flate2::Compression::fast(),
        );
        std::io::Write::write_all(&mut gz, &raw_tar).unwrap();
        gz.finish().unwrap();

        let err = extract_tar_gz(&archive, &extract_to).unwrap_err();
        assert!(matches!(err, AgentMgmtError::PathTraversal(_)));
    }

    /// 手写一个 tar 归档字节流(单 file entry,GNU long path 不启用)
    fn build_tar_with_path(name: &str, data: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        let name_bytes = name.as_bytes();
        let copy_len = name_bytes.len().min(100);
        header[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

        // mode: 0644 八进制
        let mode = b"0000644\0";
        header[100..108].copy_from_slice(mode);
        // uid/gid
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        // size
        let size_str = format!("{:011o}\0", data.len());
        header[124..136].copy_from_slice(size_str.as_bytes());
        // mtime
        header[136..148].copy_from_slice(b"00000000000\0");
        // typeflag: '0' = regular file
        header[156] = b'0';
        // magic
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");

        // 计算 checksum(checksum 字段本身按 spaces 计算)
        let mut chs = [b' '; 8];
        header[148..156].copy_from_slice(&chs);
        let sum: u32 = header.iter().map(|&b| b as u32).sum();
        let cksum = format!("{:06o}\0 ", sum);
        chs.copy_from_slice(cksum.as_bytes());
        header[148..156].copy_from_slice(&chs);

        // 数据 + padding
        let mut out = Vec::new();
        out.extend_from_slice(&header);
        out.extend_from_slice(data);
        let pad = (512 - (data.len() % 512)) % 512;
        out.extend(std::iter::repeat_n(0u8, pad));
        // 2 个 512 字节的零块作为 EOF 标记
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
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
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            zip.start_file("../../../etc/evil", opts).unwrap();
            zip.write_all(b"pwned").unwrap();
            zip.finish().unwrap();
        }

        let err = extract_zip(&archive, &extract_to).unwrap_err();
        assert!(matches!(err, AgentMgmtError::PathTraversal(_)));
    }
}
