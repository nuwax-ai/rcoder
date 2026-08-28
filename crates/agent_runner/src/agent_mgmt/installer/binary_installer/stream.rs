//! 流式入口（install_from_stream / install_from_prepared_stream）。

//! Binary installer via gRPC client streaming (P0-1)
//!
//! Protocol:
//! - **First chunk**: carries `Metadata` (agent_id, command, args, sha256)
//! - **Subsequent chunks**: carry raw `data` bytes
//!
//! The stream is accumulated into memory, then:
//! 1. Verified against SHA-256 (if provided)
//! 2. Detected by magic bytes — **only tar.gz / zip archives accepted**
//! 3. Extracted into `agent_dir` via [`archive_installer`], entrypoint copied to `bin_dir`
//!
//! ## Safety
//! - Cumulative byte count is bounded by [`shared_types::MAX_BINARY_SIZE`]
//! - Path traversal and zip bomb protections live in `archive_installer`
//! - Non-archive files (ELF / PE / script) are rejected with `UnsupportedType`

use std::pin::Pin;

use bytes::Bytes;
use futures_util::Stream;
use sha2::Digest;
use shared_types::InstallType;
use shared_types_grpc::{InstallAgentRequest, InstallAgentResponse};

use super::bytes::install_from_bytes;
use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};
use crate::agent_mgmt::path_manager::PathManager;
use crate::agent_mgmt::registry::AgentRegistry;

/// Server-side stream type (matches `tonic::Streaming<InstallAgentRequest>`).
pub type IncomingStream =
    Pin<Box<dyn Stream<Item = Result<InstallAgentRequest, tonic::Status>> + Send>>;


/// 预解析的 stream metadata（由 gRPC 层解析后传入，避免重复解析）
pub struct StreamMetadata {
    pub agent_id: String,
    pub command: String,
    pub args: Vec<String>,
    pub expected_sha256: Option<String>,
}

/// 二进制安装参数
///
/// 将 `install_from_bytes` 的业务参数封装为结构体，提升可读性。
pub struct InstallBytesParams<'a> {
    pub agent_id: &'a str,
    pub command: &'a str,
    pub args: &'a [String],
    pub expected_sha256: Option<&'a str>,
    pub install_type: InstallType,
    pub bytes: Bytes,
    /// 版本号(写入注册表,可选)
    pub version: Option<&'a str>,
    /// 源 URL(写入注册表,可选)
    pub source: Option<&'a str>,
}

/// Streaming entry point. Drains the gRPC stream, accumulates the payload,
/// then hands it off to [`install_from_bytes`] for placement.
///
/// Prefer [`install_from_prepared_stream`] when metadata is already parsed (avoids double parsing).
#[allow(dead_code)] // used in tests; external callers should prefer install_from_prepared_stream
pub async fn install_from_stream(
    registry: &AgentRegistry,
    path_manager: &PathManager,
    mut stream: IncomingStream,
) -> AgentMgmtResult<InstallAgentResponse> {
    use futures_util::StreamExt;

    // 1. 等待首包 metadata
    let first = stream
        .next()
        .await
        .ok_or(AgentMgmtError::StreamTruncated)?
        .map_err(|e| AgentMgmtError::InvalidChunk(format!("grpc stream error: {e}")))?;

    let metadata = first
        .metadata
        .ok_or_else(|| AgentMgmtError::InvalidChunk("first chunk missing metadata".into()))?;

    let agent_id = metadata
        .agent_id
        .ok_or_else(|| AgentMgmtError::InvalidChunk("metadata.agent_id missing".into()))?;
    crate::agent_mgmt::path_manager::validate_agent_id(&agent_id)
        .map_err(AgentMgmtError::InvalidManifest)?;

    let command = metadata
        .command
        .ok_or_else(|| AgentMgmtError::InvalidChunk("metadata.command missing".into()))?;
    crate::agent_mgmt::path_manager::validate_command(&command)
        .map_err(AgentMgmtError::InvalidManifest)?;

    let args: Vec<String> = metadata.args;
    let expected_sha256: Option<String> = metadata.sha256.filter(|s| !s.is_empty());

    // 2. 累积后续 chunk 到内存(限制大小)
    //    关键:首包的 data 字段若非空,也要纳入 buffer(避免客户端首包同时携带
    //    metadata + 第一段数据时被静默丢弃)。
    let mut buffer: Vec<u8> = Vec::new();
    if !first.data.is_empty() {
        if first.data.len() as u64 > shared_types::MAX_BINARY_SIZE {
            return Err(AgentMgmtError::BinaryTooLarge {
                size: first.data.len() as u64,
                max: shared_types::MAX_BINARY_SIZE,
            });
        }
        buffer.extend_from_slice(&first.data);
    }
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| AgentMgmtError::InvalidChunk(format!("grpc stream error: {e}")))?;
        if chunk.metadata.is_some() {
            return Err(AgentMgmtError::InvalidChunk(
                "metadata only allowed in first chunk".into(),
            ));
        }
        if chunk.data.is_empty() {
            continue;
        }
        if buffer.len() as u64 + chunk.data.len() as u64 > shared_types::MAX_BINARY_SIZE {
            return Err(AgentMgmtError::BinaryTooLarge {
                size: buffer.len() as u64 + chunk.data.len() as u64,
                max: shared_types::MAX_BINARY_SIZE,
            });
        }
        buffer.extend_from_slice(&chunk.data);
    }

    if buffer.is_empty() {
        return Err(AgentMgmtError::StreamTruncated);
    }

    let bytes = Bytes::from(buffer);
    install_from_bytes(
        registry,
        path_manager,
        InstallBytesParams {
            agent_id: &agent_id,
            command: &command,
            args: &args,
            expected_sha256: expected_sha256.as_deref(),
            install_type: InstallType::Binary,
            bytes,
            version: None,
            source: None,
        },
    )
    .await
}

/// 入口点：metadata 已由 gRPC 层解析，避免重复解析。
///
/// - `metadata`: 预解析的 agent_id / command / args / sha256
/// - `first_data`: 首包可能携带的数据（非空时一并纳入 buffer）
/// - `stream`: 剩余数据 chunk（不含首包）
pub async fn install_from_prepared_stream(
    registry: &AgentRegistry,
    path_manager: &PathManager,
    metadata: StreamMetadata,
    first_data: Bytes,
    mut stream: IncomingStream,
) -> AgentMgmtResult<InstallAgentResponse> {
    use futures_util::StreamExt;

    let mut buffer: Vec<u8> = Vec::new();
    if !first_data.is_empty() {
        if first_data.len() as u64 > shared_types::MAX_BINARY_SIZE {
            return Err(AgentMgmtError::BinaryTooLarge {
                size: first_data.len() as u64,
                max: shared_types::MAX_BINARY_SIZE,
            });
        }
        buffer.extend_from_slice(&first_data);
    }
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| AgentMgmtError::InvalidChunk(format!("grpc stream error: {e}")))?;
        if chunk.metadata.is_some() {
            return Err(AgentMgmtError::InvalidChunk(
                "metadata only allowed in first chunk".into(),
            ));
        }
        if chunk.data.is_empty() {
            continue;
        }
        if buffer.len() as u64 + chunk.data.len() as u64 > shared_types::MAX_BINARY_SIZE {
            return Err(AgentMgmtError::BinaryTooLarge {
                size: buffer.len() as u64 + chunk.data.len() as u64,
                max: shared_types::MAX_BINARY_SIZE,
            });
        }
        buffer.extend_from_slice(&chunk.data);
    }

    if buffer.is_empty() {
        return Err(AgentMgmtError::StreamTruncated);
    }

    let bytes = Bytes::from(buffer);
    install_from_bytes(
        registry,
        path_manager,
        InstallBytesParams {
            agent_id: &metadata.agent_id,
            command: &metadata.command,
            args: &metadata.args,
            expected_sha256: metadata.expected_sha256.as_deref(),
            install_type: InstallType::Binary,
            bytes,
            version: None,
            source: None,
        },
    )
    .await
}

