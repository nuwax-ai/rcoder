//! 文件路径入口（install_from_file，避免全量读入内存）。

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

use futures_util::Stream;
use sha2::Digest;
use shared_types_grpc::{InstallAgentRequest, InstallAgentResponse};
use tracing::{debug, info, warn};

use super::bytes::InstallFileParams;
use super::staging::{StagingInstallParams, _install_from_staging, detect_file_type};
use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};
use crate::agent_mgmt::path_manager::PathManager;
use crate::agent_mgmt::registry::AgentRegistry;

/// Server-side stream type (matches `tonic::Streaming<InstallAgentRequest>`).
pub type IncomingStream =
    Pin<Box<dyn Stream<Item = Result<InstallAgentRequest, tonic::Status>> + Send>>;


/// 文件路径安装入口（避免全量读入内存）。
///
/// 适用于 URL 下载等已有磁盘文件的场景：
/// 1. 读取前 4 字节 magic bytes 检测文件类型
/// 2. `std::fs::metadata` 校验文件大小
/// 3. rename 到 staging 位置（同文件系统零拷贝）
/// 4. 调用共享解压/注册逻辑
pub async fn install_from_file(
    registry: &AgentRegistry,
    path_manager: &PathManager,
    params: InstallFileParams<'_>,
) -> AgentMgmtResult<InstallAgentResponse> {
    let InstallFileParams {
        agent_id,
        command,
        args,
        install_type,
        download_path,
        version: param_version,
        source: param_source,
        file_size,
    } = params;

    crate::agent_mgmt::path_manager::validate_agent_id(agent_id)
        .map_err(AgentMgmtError::InvalidManifest)?;
    crate::agent_mgmt::path_manager::validate_command(command)
        .map_err(AgentMgmtError::InvalidManifest)?;

    // 1. 文件大小校验（metadata 级别，不读入内存）
    if file_size > shared_types::MAX_BINARY_SIZE {
        return Err(AgentMgmtError::BinaryTooLarge {
            size: file_size,
            max: shared_types::MAX_BINARY_SIZE,
        });
    }

    // 2. 读取前 4 字节 magic bytes 检测文件类型
    let mut header = [0u8; 4];
    {
        use std::io::Read;
        let mut f = std::fs::File::open(download_path).map_err(AgentMgmtError::Io)?;
        f.read_exact(&mut header).map_err(AgentMgmtError::Io)?;
    }
    let file_type = detect_file_type(&header);
    if file_type != "tar.gz" && file_type != "zip" {
        return Err(AgentMgmtError::UnsupportedType(format!(
            "only tar.gz and zip archives are supported, got: {file_type}"
        )));
    }

    // 3. 准备 staging 目录 + rename 文件
    let t0 = std::time::Instant::now();
    path_manager.ensure_dirs().await?;
    debug!(
        "[agent_mgmt] install_from_file: ensure_dirs took {:?}",
        t0.elapsed()
    );

    // 使用版本目录（如果提供了版本号）
    let t1 = std::time::Instant::now();
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
    if version_dir.exists() {
        debug!("[agent_mgmt] install_from_file: removing existing version_dir");
        if let Err(e) = tokio::fs::remove_dir_all(&version_dir).await {
            warn!(
                "[agent_mgmt] failed to remove existing version_dir {}: {e}",
                version_dir.display()
            );
        }
        info!(
            "[agent_mgmt] install_from_file: remove_dir_all took {:?}",
            t1.elapsed()
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
    // rename（同文件系统零拷贝）或 copy（跨文件系统降级）
    let t2 = std::time::Instant::now();
    if tokio::fs::rename(download_path, &staging).await.is_err() {
        debug!("[agent_mgmt] install_from_file: rename failed, falling back to copy");
        tokio::fs::copy(download_path, &staging).await?;
        if let Err(e) = tokio::fs::remove_file(download_path).await {
            warn!(
                "[agent_mgmt] install_from_file: failed to remove source after copy fallback: path={}, error={}",
                download_path.display(),
                e
            );
        }
    }
    debug!(
        "[agent_mgmt] install_from_file: staging file ready, took {:?}",
        t2.elapsed()
    );

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

