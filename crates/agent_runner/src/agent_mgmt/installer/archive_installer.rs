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

    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.into_owned();

        let sanitized = sanitize_entry_path(&entry_path)?;

        let dest_path = dest_dir.join(&sanitized);

        // 先创建父目录,确保 canonicalize 能解析(避免 macOS 上 /var -> /private 软链错位)
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        ensure_within(&dest_path, dest_dir)?;

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
            std::fs::create_dir_all(&dest_path)?;
        } else if entry_type.is_file() {
            file_count += 1;
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = File::create(&dest_path)?;
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = entry.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
            }
            out.sync_all()?;

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
                entry_type, entry_path.display()
            );
        }
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

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| AgentMgmtError::Archive(format!("read zip entry {i}: {e}")))?;
        let entry_name = entry.name().to_string();
        let entry_path = PathBuf::from(&entry_name);

        let sanitized = sanitize_entry_path(&entry_path)?;
        let dest_path = dest_dir.join(&sanitized);
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        ensure_within(&dest_path, dest_dir)?;

        let entry_size = entry.size();
        total_uncompressed = total_uncompressed.saturating_add(entry_size);
        if total_uncompressed > MAX_EXTRACTED_SIZE {
            return Err(AgentMgmtError::ArchiveBomb {
                size: total_uncompressed,
                max: MAX_EXTRACTED_SIZE,
            });
        }

        if entry.is_dir() {
            std::fs::create_dir_all(&dest_path)?;
        } else if entry.is_file() {
            file_count += 1;
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = File::create(&dest_path)?;
            std::io::copy(&mut entry, &mut out)?;
            out.sync_all()?;

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

    Ok(file_count)
}

/// Locate the entry executable in `extract_dir`.
///
/// Checks exact paths only (no ambiguous fallback):
/// 1. `<extract_dir>/<command>`
/// 2. `<extract_dir>/bin/<command>`
pub fn find_entrypoint(extract_dir: &Path, command: &str) -> Option<PathBuf> {
    let direct = extract_dir.join(command);
    if is_executable_file(&direct) {
        return Some(direct);
    }

    let in_bin = extract_dir.join("bin").join(command);
    if is_executable_file(&in_bin) {
        return Some(in_bin);
    }

    None
}

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
    // 在 macOS / Linux 上 canonicalize 会解析符号链接,且需要文件存在。
    // 这里 dest_path 的父目录可能尚未创建(dest_dir 一定存在),
    // 所以我们用 cleaned() 进行路径规范化,然后对 dest_path 的父目录做 canonicalize。
    let base_canon = base
        .canonicalize()
        .unwrap_or_else(|_| base.to_path_buf());

    let parent = dest_path.parent().unwrap_or(dest_path);
    let canon_parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());

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
            tar.append_data(&mut header, "bin/hello", &data[..]).unwrap();

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
        out.extend(std::iter::repeat(0u8).take(pad));
        // 2 个 512 字节的零块作为 EOF 标记
        out.extend(std::iter::repeat(0u8).take(1024));
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
