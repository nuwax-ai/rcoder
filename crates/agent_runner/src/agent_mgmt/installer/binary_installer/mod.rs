//! 二进制 agent 安装器（目录化：stream/bytes/file 入口 + staging 共享核心）。
//!
//! 入口变体（准备 staging + 校验大小/sha256）全部委托 `_install_from_staging`
//! 核心（spawn_blocking 解压→normalize→manifest 注册）。

mod bytes;
mod file;
mod staging;
mod stream;

pub use bytes::{IncomingStream, InstallFileParams, install_from_bytes};
pub use file::install_from_file;
pub use stream::{InstallBytesParams, StreamMetadata, install_from_prepared_stream, install_from_stream};

#[cfg(test)]
mod tests {
    use ::bytes::Bytes as BytesBytes;
    use shared_types::InstallType;
    use shared_types_grpc::InstallAgentRequest;

    use super::staging::detect_file_type;
    use crate::agent_mgmt::error::AgentMgmtError;
    use super::*;
    use crate::agent_mgmt::path_manager::PathManager;
    use crate::agent_mgmt::registry::AgentRegistry;
    use tempfile::tempdir;

    fn fixture_pm() -> PathManager {
        let tmp = tempdir().unwrap();
        PathManager::new_with_root(tmp.path().to_path_buf())
    }

    fn fixture_registry(pm: &PathManager) -> AgentRegistry {
        AgentRegistry::empty(pm.clone())
    }

    #[test]
    fn detect_elf() {
        assert_eq!(detect_file_type(b"\x7FELF\x02\x01\x01\x00"), "elf");
    }

    #[test]
    fn detect_gzip() {
        assert_eq!(
            detect_file_type(b"\x1F\x8B\x08\x00\x00\x00\x00\x00"),
            "tar.gz"
        );
    }

    #[test]
    fn detect_zip() {
        assert_eq!(detect_file_type(b"PK\x03\x04\x14\x00"), "zip");
    }

    #[test]
    fn detect_shebang() {
        assert_eq!(detect_file_type(b"#!/bin/sh\necho hi\n"), "script");
    }

    #[tokio::test]
    async fn install_rejects_non_archive() {
        let pm = fixture_pm();
        let r = fixture_registry(&pm);
        let payload = b"\x7FELF\x00\x01\x02\x03\x04\x05\x06\x07\x08fake-binary-bytes";

        let err = install_from_bytes(
            &r,
            &pm,
            InstallBytesParams {
                agent_id: "fake-agent",
                command: "fake-agent",
                args: &[],
                expected_sha256: None,
                install_type: InstallType::Binary,
                bytes: BytesBytes::from_static(payload),
                version: None,
                source: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AgentMgmtError::UnsupportedType(_)));
    }

    #[tokio::test]
    async fn install_rejects_oversize() {
        let pm = fixture_pm();
        let r = fixture_registry(&pm);
        // MAX_BINARY_SIZE + 1 字节
        let payload = vec![0u8; shared_types::MAX_BINARY_SIZE as usize + 1];
        let err = install_from_bytes(
            &r,
            &pm,
            InstallBytesParams {
                agent_id: "huge",
                command: "huge",
                args: &[],
                expected_sha256: None,
                install_type: InstallType::Binary,
                bytes: BytesBytes::from(payload),
                version: None,
                source: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AgentMgmtError::BinaryTooLarge { .. }));
    }

    /// 构建一个包含单个脚本文件的最小 tar.gz 包
    fn build_minimal_tar_gz(command: &str, script_body: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            let mut header = tar::Header::new_gnu();
            header.set_size(script_body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, command, script_body).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        buf
    }

    #[tokio::test]
    async fn install_rejects_checksum_mismatch() {
        let pm = fixture_pm();
        let r = fixture_registry(&pm);
        let payload = build_minimal_tar_gz("x", b"#!/bin/sh\necho hi\n");
        let err = install_from_bytes(
            &r,
            &pm,
            InstallBytesParams {
                agent_id: "x",
                command: "x",
                args: &[],
                expected_sha256: Some(
                    "0000000000000000000000000000000000000000000000000000000000000000",
                ),
                install_type: InstallType::Binary,
                bytes: BytesBytes::from(payload),
                version: None,
                source: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, AgentMgmtError::ChecksumMismatch { .. }));
    }

    #[tokio::test]
    async fn install_accepts_matching_checksum() {
        let pm = fixture_pm();
        let r = fixture_registry(&pm);
        let script = b"#!/bin/sh\necho hello\n";
        let payload = build_minimal_tar_gz("hello", script);
        let actual_sha = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(&payload);
            hex::encode(h.finalize())
        };
        let resp = install_from_bytes(
            &r,
            &pm,
            InstallBytesParams {
                agent_id: "hello",
                command: "hello",
                args: &[],
                expected_sha256: Some(&actual_sha),
                install_type: InstallType::Binary,
                bytes: BytesBytes::from(payload),
                version: None,
                source: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(resp.agent_id, "hello");
        assert_eq!(resp.file_type, "tar.gz");
    }

    /// 验证:streaming 模式下,首包同时携带 metadata + data 时,data 不能被丢弃
    #[tokio::test]
    async fn install_from_stream_preserves_first_chunk_data() {
        use futures_util::stream;
        use shared_types_grpc::install_agent_request::Metadata;

        let pm = fixture_pm();
        let r = fixture_registry(&pm);

        // 构建真实 tar.gz,然后拆成两段验证首包 data 不丢失
        let archive = build_minimal_tar_gz("stream-first-data", b"#!/bin/sh\necho ok\n");
        let split = archive.len() / 3; // 第一段约 1/3
        let first_data = archive[..split].to_vec();
        let second_data = archive[split..].to_vec();

        let first_chunk = InstallAgentRequest {
            metadata: Some(Metadata {
                agent_id: Some("stream-first-data".into()),
                command: Some("stream-first-data".into()),
                args: vec![],
                sha256: None,
                install_type: None,
                source_url: None,
                npm_package: None,
                version: None,
                platforms: None,
                force: None,
            }),
            data: first_data,
        };
        let second_chunk = InstallAgentRequest {
            metadata: None,
            data: second_data,
        };
        let s: IncomingStream = Box::pin(stream::iter(vec![Ok(first_chunk), Ok(second_chunk)]));

        let resp = install_from_stream(&r, &pm, s).await.unwrap();
        assert_eq!(resp.agent_id, "stream-first-data");
        assert_eq!(resp.file_type, "tar.gz");
    }

    /// 直接测试 install_from_prepared_stream：metadata 已预解析，数据通过 stream 传入
    #[tokio::test]
    async fn prepared_stream_installs_successfully() {
        use futures_util::stream;

        let pm = fixture_pm();
        let r = fixture_registry(&pm);

        let archive = build_minimal_tar_gz("prepared-cmd", b"#!/bin/sh\necho prepared\n");

        let metadata = StreamMetadata {
            agent_id: "prepared-agent".into(),
            command: "prepared-cmd".into(),
            args: vec!["--flag".into()],
            expected_sha256: None,
        };
        // 模拟:首包 data 为空，全部数据在后续 chunk
        let s: IncomingStream = Box::pin(stream::iter(vec![Ok(InstallAgentRequest {
            metadata: None,
            data: archive,
        })]));

        let resp = install_from_prepared_stream(&r, &pm, metadata, BytesBytes::new(), s)
            .await
            .unwrap();
        assert_eq!(resp.agent_id, "prepared-agent");
        assert_eq!(resp.file_type, "tar.gz");
        assert!(resp.file_size > 0);
    }

    /// prepared_stream: 首包 data + 后续 chunk 组合正确
    #[tokio::test]
    async fn prepared_stream_combines_first_data_and_chunks() {
        use futures_util::stream;

        let pm = fixture_pm();
        let r = fixture_registry(&pm);

        let archive = build_minimal_tar_gz("combo-cmd", b"#!/bin/sh\necho combo\n");
        let mid = archive.len() / 2;
        let first_data = BytesBytes::from(archive[..mid].to_vec());
        let rest_data = archive[mid..].to_vec();

        let metadata = StreamMetadata {
            agent_id: "combo-agent".into(),
            command: "combo-cmd".into(),
            args: vec![],
            expected_sha256: None,
        };
        let s: IncomingStream = Box::pin(stream::iter(vec![Ok(InstallAgentRequest {
            metadata: None,
            data: rest_data,
        })]));

        let resp = install_from_prepared_stream(&r, &pm, metadata, first_data, s)
            .await
            .unwrap();
        assert_eq!(resp.agent_id, "combo-agent");
        assert_eq!(resp.file_type, "tar.gz");
    }

    /// prepared_stream: 空数据应返回 StreamTruncated
    #[tokio::test]
    async fn prepared_stream_rejects_empty_data() {
        use futures_util::stream;

        let pm = fixture_pm();
        let r = fixture_registry(&pm);

        let metadata = StreamMetadata {
            agent_id: "empty-agent".into(),
            command: "empty-cmd".into(),
            args: vec![],
            expected_sha256: None,
        };
        let s: IncomingStream = Box::pin(stream::iter(vec![]));

        let err = install_from_prepared_stream(&r, &pm, metadata, BytesBytes::new(), s)
            .await
            .unwrap_err();
        assert!(matches!(err, AgentMgmtError::StreamTruncated));
    }
}
