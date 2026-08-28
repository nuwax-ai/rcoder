//! 字节入口（install_from_bytes）。

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

use std::path::Path;
use std::pin::Pin;

use futures_util::Stream;
use sha2::{Digest, Sha256};
use shared_types::InstallType;
use shared_types_grpc::{InstallAgentRequest, InstallAgentResponse};
use tracing::warn;

use super::staging::{StagingInstallParams, _install_from_staging, detect_file_type};
use super::stream::InstallBytesParams;
use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};
use crate::agent_mgmt::path_manager::PathManager;
use crate::agent_mgmt::registry::AgentRegistry;

/// Server-side stream type (matches `tonic::Streaming<InstallAgentRequest>`).
pub type IncomingStream =
    Pin<Box<dyn Stream<Item = Result<InstallAgentRequest, tonic::Status>> + Send>>;


/// 文件路径安装参数（避免全量读入内存）
///
/// 用于 `install_from_file`，接受已下载好的文件路径。
pub struct InstallFileParams<'a> {
    pub agent_id: &'a str,
    pub command: &'a str,
    pub args: &'a [String],
    pub install_type: InstallType,
    /// 已下载的文件路径（会被 rename 到 staging 位置）
    pub download_path: &'a Path,
    /// 版本号(写入注册表,可选)
    pub version: Option<&'a str>,
    /// 源 URL(写入注册表,可选)
    pub source: Option<&'a str>,
    /// 文件大小(下载阶段已知,写入注册表)
    pub file_size: u64,
}

/// Byte-based entry point. Testable without a gRPC stream.
pub async fn install_from_bytes(
    registry: &AgentRegistry,
    path_manager: &PathManager,
    params: InstallBytesParams<'_>,
) -> AgentMgmtResult<InstallAgentResponse> {
    let InstallBytesParams {
        agent_id,
        command,
        args,
        expected_sha256,
        install_type,
        bytes,
        version: param_version,
        source: param_source,
    } = params;

    crate::agent_mgmt::path_manager::validate_agent_id(agent_id)
        .map_err(AgentMgmtError::InvalidManifest)?;
    crate::agent_mgmt::path_manager::validate_command(command)
        .map_err(AgentMgmtError::InvalidManifest)?;

    if (bytes.len() as u64) > shared_types::MAX_BINARY_SIZE {
        return Err(AgentMgmtError::BinaryTooLarge {
            size: bytes.len() as u64,
            max: shared_types::MAX_BINARY_SIZE,
        });
    }

    // 1. SHA-256 校验
    if let Some(expected) = expected_sha256 {
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let actual = hex::encode(hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(AgentMgmtError::ChecksumMismatch {
                expected: expected.to_string(),
                actual,
            });
        }
    }

    // 2. 文件类型检测(仅接受压缩包:tar.gz / zip)
    let file_type = detect_file_type(&bytes);
    if file_type != "tar.gz" && file_type != "zip" {
        return Err(AgentMgmtError::UnsupportedType(format!(
            "only tar.gz and zip archives are supported, got: {file_type}"
        )));
    }

    // 3. 写入 staging 文件
    path_manager.ensure_dirs().await?;

    // 使用版本目录（如果提供了版本号）
    let version_dir = if let Some(version) = param_version {
        path_manager
            .agent_version_dir(agent_id, version)
            .map_err(AgentMgmtError::InvalidManifest)?
    } else {
        path_manager
            .agent_dir(agent_id)
            .map_err(AgentMgmtError::InvalidManifest)?
    };

    // 只删除特定版本目录，不影响其他版本
    if version_dir.exists()
        && let Err(e) = tokio::fs::remove_dir_all(&version_dir).await
    {
        warn!(
            "[agent_mgmt] failed to remove existing version_dir {}: {e}",
            version_dir.display()
        );
    }
    tokio::fs::create_dir_all(&version_dir).await?;

    let staging_ext = match file_type.as_str() {
        "tar.gz" => "tar.gz",
        "zip" => "zip",
        other => {
            return Err(AgentMgmtError::InstallFailed(format!(
                "unsupported file_type: {other}"
            )));
        }
    };
    let staging = version_dir.join(format!("staging.{staging_ext}"));
    tokio::fs::write(&staging, &bytes).await?;
    let file_size = bytes.len() as u64;

    // 4. 共享解压/注册逻辑
    _install_from_staging(
        registry,
        &staging,
        &version_dir,
        StagingInstallParams {
            agent_id,
            command,
            args,
            install_type,
            file_type,
            file_size,
            version: param_version,
            source: param_source,
        },
    )
    .await
}

