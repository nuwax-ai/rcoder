//! Archive extraction with safety guarantees (P0-1)
//!
//! **薄封装层**。实现体在 [`download_utils::archive`]——此前本文件与它是两份约 90%
//! 逐行相同的副本（各 ~630 行，5 个同名公开函数 + 3 个同名私有 helper），path
//! traversal 之类的绕过若被发现要改两遍。现已收敛为单头。
//!
//! 本层只承担 agent 安装场景特有的两件事：
//! 1. **强制 zip-bomb 配额** [`shared_types::MAX_EXTRACTED_SIZE`]——agent 安装包来自
//!    外部 URL/npm，与 `download_utils` 那些"解压发生在用户隔离容器内、由容器配额
//!    作主边界"的调用点风险模型不同，故这里传 `Some(max)` 而非 `None`。
//! 2. **错误映射**为 [`AgentMgmtError`]，保持上层错误码契约不变
//!    （见 [`crate::agent_mgmt::error`] 里的 `From<ArchiveError>`）。
//!
//! 两种格式：`tar.gz` 与 `zip`。Path traversal 防护（`..` 分量、绝对路径、Windows
//! 盘符、NUL 字节）在 `download_utils::archive` 内**始终生效**，不受配额参数影响。
//! 非归档文件由 `binary_installer` 上游以 `UnsupportedType` 拒绝。

use std::path::{Path, PathBuf};

use download_utils::archive;
use download_utils::archive::ArchiveError;
use shared_types::MAX_EXTRACTED_SIZE;

use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};

/// Extract a `.tar.gz` archive into `dest_dir`.
///
/// Validates every entry:
/// 1. No `..` components, no absolute paths, no NUL bytes
/// 2. Cumulative uncompressed size <= [`MAX_EXTRACTED_SIZE`]
///
/// Returns the number of file entries extracted.
pub fn extract_tar_gz(archive_path: &Path, dest_dir: &Path) -> AgentMgmtResult<usize> {
    Ok(archive::extract_tar_gz(
        archive_path,
        dest_dir,
        Some(MAX_EXTRACTED_SIZE),
    )?)
}

/// Extract a `.zip` archive into `dest_dir`.
///
/// 校验规则同 [`extract_tar_gz`]。
pub fn extract_zip(archive_path: &Path, dest_dir: &Path) -> AgentMgmtResult<usize> {
    Ok(archive::extract_zip(
        archive_path,
        dest_dir,
        Some(MAX_EXTRACTED_SIZE),
    )?)
}

/// Locate the entrypoint executable under `extract_dir`.
pub fn find_entrypoint(extract_dir: &Path, command: &str) -> Option<PathBuf> {
    archive::find_entrypoint(extract_dir, command)
}

/// Strip a single wrapper directory (e.g. `deepagents-dev-templates-0.2.9/`).
///
/// **本函数是这次去重里唯一一处 wire 可见的错误码变更，特此明写。**
/// 原实现的 normalize 失败分两种码：rename 失败 → `InstallFailed`
/// （`ERR_AGENT_MGMT_INSTALL_FAILED`），而 `read_dir` 失败经 `?` → `Io`
/// （`ERR_INTERNAL_SERVER_ERROR`）。`download_utils` 把两者都收敛成
/// `ArchiveError::Io`，封装层已无从区分，故统一映射为 `InstallFailed`：
/// - rename 路径（较常见：跨设备/权限/目标已存在）错误码**不变**
/// - `read_dir` 路径（罕见边界：解压刚成功后目录即不可读）由
///   `ERR_INTERNAL_SERVER_ERROR` 变为 `ERR_AGENT_MGMT_INSTALL_FAILED`
///
/// 不为复刻这点差异给 `ArchiveError` 加变体：同一个"安装中剥壳步骤失败"原本返回两个
/// 码本身就是不一致，`INSTALL_FAILED` 对调用方也更有信息量（安装确实失败了）。
pub fn normalize_extracted_dir(agent_dir: &Path) -> AgentMgmtResult<bool> {
    archive::normalize_extracted_dir(agent_dir).map_err(|e| match e {
        ArchiveError::Io(io) => AgentMgmtError::InstallFailed(format!(
            "normalize extracted dir {}: {io}",
            agent_dir.display()
        )),
        other => other.into(),
    })
}

/// Read the entrypoint declared by package metadata (Node.js / Bun / Python 等目录型包)。
pub fn find_entrypoint_from_metadata(agent_dir: &Path) -> Option<(String, Vec<String>)> {
    archive::find_entrypoint_from_metadata(agent_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    /// 手写一个 tar 归档字节流(单 file entry,GNU long path 不启用)
    ///
    /// tar crate 的 `set_path` 自身会拒绝 `..` 路径，故手写 tar 头绕过 Builder
    /// 校验，才能验证**我们的** extract 逻辑确实拦得下。
    fn build_tar_with_path(name: &str, data: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        let name_bytes = name.as_bytes();
        let copy_len = name_bytes.len().min(100);
        header[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

        // mode: 0644 八进制
        header[100..108].copy_from_slice(b"0000644\0");
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

    fn write_tar_gz(path: &Path, raw_tar: &[u8]) {
        let mut gz = flate2::write::GzEncoder::new(
            std::fs::File::create(path).unwrap(),
            flate2::Compression::fast(),
        );
        Write::write_all(&mut gz, raw_tar).unwrap();
        gz.finish().unwrap();
    }

    /// 本层的职责之一是错误映射：traversal 必须以 `AgentMgmtError::PathTraversal`
    /// 出现（→ `ERR_AGENT_MGMT_PATH_TRAVERSAL`），而非透传 `ArchiveError`。
    #[test]
    fn extract_tar_gz_maps_path_traversal_to_agent_mgmt_error() {
        let tmp = tempdir().unwrap();
        let archive_path = tmp.path().join("evil.tar.gz");
        let extract_to = tmp.path().join("out");
        std::fs::create_dir_all(&extract_to).unwrap();

        write_tar_gz(
            &archive_path,
            &build_tar_with_path("../../../etc/evil", b"pwned"),
        );

        let err = extract_tar_gz(&archive_path, &extract_to).unwrap_err();
        assert!(matches!(err, AgentMgmtError::PathTraversal(_)));
        assert_eq!(
            err.error_code(),
            shared_types::error_codes::ERR_AGENT_MGMT_PATH_TRAVERSAL
        );
    }

    #[test]
    fn extract_zip_maps_path_traversal_to_agent_mgmt_error() {
        let tmp = tempdir().unwrap();
        let archive_path = tmp.path().join("evil.zip");
        let extract_to = tmp.path().join("out");
        std::fs::create_dir_all(&extract_to).unwrap();

        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            zip.start_file("../../../etc/evil", opts).unwrap();
            zip.write_all(b"pwned").unwrap();
            zip.finish().unwrap();
        }

        let err = extract_zip(&archive_path, &extract_to).unwrap_err();
        assert!(matches!(err, AgentMgmtError::PathTraversal(_)));
    }

    /// 本层的另一职责：强制配额。`download_utils` 侧默认不限额，本封装必须传
    /// `Some(MAX_EXTRACTED_SIZE)`，超限要映射成 `ArchiveBomb`
    /// （→ `ERR_AGENT_MGMT_ARCHIVE_BOMB`）。
    ///
    /// 用 tar header 声明的 size 触发：header 写一个远超配额的长度、实际数据留空，
    /// 累计在写盘前即命中上限（无需真造 1GB 数据）。
    #[test]
    fn extract_tar_gz_enforces_zip_bomb_quota() {
        let tmp = tempdir().unwrap();
        let archive_path = tmp.path().join("bomb.tar.gz");
        let extract_to = tmp.path().join("out");
        std::fs::create_dir_all(&extract_to).unwrap();

        // 构造 header.size = MAX + 1 的单条目 tar
        let mut raw = build_tar_with_path("big.bin", b"");
        let oversized = MAX_EXTRACTED_SIZE + 1;
        let size_str = format!("{:011o}\0", oversized);
        raw[124..136].copy_from_slice(size_str.as_bytes());
        // size 变了须重算 checksum
        let mut chs = [b' '; 8];
        raw[148..156].copy_from_slice(&chs);
        let header_sum: u32 = raw[..512].iter().map(|&b| b as u32).sum();
        let cksum = format!("{:06o}\0 ", header_sum);
        chs.copy_from_slice(cksum.as_bytes());
        raw[148..156].copy_from_slice(&chs);
        write_tar_gz(&archive_path, &raw);

        let err = extract_tar_gz(&archive_path, &extract_to).unwrap_err();
        match err {
            AgentMgmtError::ArchiveBomb { size, max } => {
                assert_eq!(max, MAX_EXTRACTED_SIZE);
                assert!(size > MAX_EXTRACTED_SIZE);
            }
            // tar crate 可能先因声明 size 与实际数据不符而报 IO/格式错——
            // 两条路径都算拦下了，但不能是 Ok
            other => panic!("expected ArchiveBomb, got {other:?}"),
        }
    }

    /// normalize 失败统一判为 `ERR_AGENT_MGMT_INSTALL_FAILED`。
    ///
    /// 这里用不存在的目录触发的是 `read_dir` 失败路径——**该路径原先返回的是
    /// `Io`/`ERR_INTERNAL_SERVER_ERROR`**，统一后变了（见函数文档，是本次重构
    /// 唯一一处 wire 可见错误码变更）。本测试把这个新契约钉住，避免将来无意漂移。
    #[test]
    fn normalize_failure_maps_to_install_failed() {
        let tmp = tempdir().unwrap();
        // 不存在的路径 → normalize 内部 IO 失败
        let missing = tmp.path().join("does-not-exist");
        let err = normalize_extracted_dir(&missing).unwrap_err();
        assert!(matches!(err, AgentMgmtError::InstallFailed(_)));
        assert_eq!(
            err.error_code(),
            shared_types::error_codes::ERR_AGENT_MGMT_INSTALL_FAILED
        );
    }

    /// 薄封装不得改变正常路径行为：round-trip 仍解出文件。
    #[test]
    fn extract_tar_gz_round_trip_still_works() {
        let tmp = tempdir().unwrap();
        let archive_path = tmp.path().join("src.tar.gz");
        let extract_to = tmp.path().join("out");
        std::fs::create_dir_all(&extract_to).unwrap();

        {
            let tar_file = std::fs::File::create(&archive_path).unwrap();
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

        let count = extract_tar_gz(&archive_path, &extract_to).unwrap();
        assert_eq!(count, 1);
        let hello = extract_to.join("bin/hello");
        assert!(hello.exists());
        assert!(std::fs::read_to_string(&hello).unwrap().contains("echo hi"));
    }
}
