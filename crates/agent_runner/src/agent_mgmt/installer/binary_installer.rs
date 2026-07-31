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

use bytes::Bytes;
use futures_util::Stream;
use sha2::{Digest, Sha256};
use shared_types::InstallType;
use shared_types_grpc::{InstallAgentRequest, InstallAgentResponse};
use tracing::{debug, info, warn};

use super::archive_installer;
use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};
use crate::agent_mgmt::installer::AgentManifest;
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
        let _ = tokio::fs::remove_file(download_path).await;
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

/// staging 安装参数
struct StagingInstallParams<'a> {
    agent_id: &'a str,
    command: &'a str,
    args: &'a [String],
    install_type: InstallType,
    file_type: String,
    file_size: u64,
    version: Option<&'a str>,
    source: Option<&'a str>,
}

/// 共享的 staging → 解压 → 注册逻辑。
///
/// 被 `install_from_bytes` 和 `install_from_file` 共同调用。
/// staging 文件在解压完成后被清理。
///
/// ## 安装模式
///
/// 解压后自动检测包类型：
/// - **目录型包**：存在 `agent-package.json` 或 `package.json`（含 `bin.start`）
///   → 整个目录保持完整，`binary_path` = agent_dir
/// - **二进制包**：无 metadata 文件
///   → 查找入口可执行文件，`binary_path` = entrypoint 路径（不复制到 bin/）
async fn _install_from_staging(
    registry: &AgentRegistry,
    staging: &Path,
    agent_dir: &Path,
    params: StagingInstallParams<'_>,
) -> AgentMgmtResult<InstallAgentResponse> {
    let StagingInstallParams {
        agent_id,
        command,
        args,
        install_type,
        file_type,
        file_size,
        version: param_version,
        source: param_source,
    } = params;
    // 同步解压 + entrypoint 查找放到阻塞线程(避免 tar/zip IO 阻塞 tokio runtime)
    let command = command.to_string();
    let command_for_block = command.clone();
    let agent_dir_clone = agent_dir.to_path_buf();
    let staging_clone = staging.to_path_buf();
    let file_type_clone = file_type.clone();
    // spawn_blocking 返回 (binary_path, file_count, resolved_args)
    // resolved_args: 目录型包从 metadata 解析的 args（如 ["dist/index.js"]），二进制型为 None
    let t3 = std::time::Instant::now();
    let spawn_result = tokio::task::spawn_blocking(move || {
        debug!(
            "[agent_mgmt] spawn_blocking started, queue wait: {:?}",
            t3.elapsed()
        );
        let t_extract = std::time::Instant::now();
        let count = match file_type_clone.as_str() {
            "tar.gz" => archive_installer::extract_tar_gz(&staging_clone, &agent_dir_clone)?,
            "zip" => archive_installer::extract_zip(&staging_clone, &agent_dir_clone)?,
            _ => {
                return Err(AgentMgmtError::InstallFailed(
                    "unsupported file_type".to_string(),
                ));
            }
        };
        debug!(
            "[agent_mgmt] extraction done: {} files, took {:?}",
            count,
            t_extract.elapsed()
        );
        let _ = std::fs::remove_file(&staging_clone);

        // 剥掉单个顶层目录包装（如 deepagents-dev-templates-0.2.9/）
        archive_installer::normalize_extracted_dir(&agent_dir_clone)?;

        // 尝试从 metadata 读取入口（目录型包：Node.js / Bun / Python 等）
        if let Some((entrypoint_script, meta_args)) =
            archive_installer::find_entrypoint_from_metadata(&agent_dir_clone)
        {
            let entrypoint_path = agent_dir_clone.join(&entrypoint_script);
            if !entrypoint_path.exists() {
                return Err(AgentMgmtError::InstallFailed(format!(
                    "entrypoint '{}' declared in package metadata but not found at {}",
                    entrypoint_script,
                    entrypoint_path.display()
                )));
            }
            // 目录型包：binary_path 指向 agent 目录本身，返回 metadata args
            let mut resolved_args = vec![entrypoint_script];
            resolved_args.extend(meta_args);
            return Ok::<(String, usize, Option<Vec<String>>), AgentMgmtError>((
                agent_dir_clone.to_string_lossy().to_string(),
                count,
                Some(resolved_args),
            ));
        }

        // 二进制型包：查找入口可执行文件
        let entrypoint = archive_installer::find_entrypoint(&agent_dir_clone, &command_for_block)
            .ok_or_else(|| {
            AgentMgmtError::InstallFailed(format!(
                "could not find entrypoint '{command_for_block}' in extracted archive"
            ))
        })?;
        // binary_path 直接指向入口文件（不复制到 bin/，由 Dockerfile PATH 配置解决查找）
        Ok::<(String, usize, Option<Vec<String>>), AgentMgmtError>((
            entrypoint.to_string_lossy().to_string(),
            count,
            None,
        ))
    })
    .await
    .map_err(|e| AgentMgmtError::InstallFailed(format!("extraction task panicked: {e}")));

    // 清理 staging 文件(如果 spawn_blocking 没删干净)
    tokio::fs::remove_file(staging).await.ok();

    // 如果解压/查找失败，清理残留的 agent_dir
    let (binary_path_str, file_count, resolved_args) = match spawn_result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tokio::fs::remove_dir_all(agent_dir).await.ok();
            return Err(e);
        }
        Err(outer) => {
            tokio::fs::remove_dir_all(agent_dir).await.ok();
            return Err(outer);
        }
    };

    // 目录型包：如果用户未提供 args，使用 metadata 解析的 args（如 ["dist/index.js"]）
    let final_args = if args.is_empty() {
        resolved_args.unwrap_or_default()
    } else {
        args.to_vec()
    };

    // 注册到注册表
    let manifest = AgentManifest {
        agent_id: agent_id.to_string(),
        install_type,
        command: command.clone(),
        args: final_args,
        binary_path: binary_path_str.clone(),
        source: param_source.map(String::from),
        version: param_version.map(String::from),
        file_size,
        file_type: file_type.clone(),
        installed_at: chrono::Utc::now().timestamp(),
    };
    manifest.validate()?;
    registry.upsert(manifest)?;

    info!(
        "[agent_mgmt] Installed: agent_id={}, command={}, binary_path={}, file_type={}, size={}",
        agent_id, command, binary_path_str, file_type, file_size
    );

    Ok(InstallAgentResponse {
        agent_id: agent_id.to_string(),
        status: shared_types_grpc::AgentInstallStatus::Available as i32,
        binary_path: binary_path_str,
        file_type,
        file_count: Some(file_count.try_into().unwrap_or(i32::MAX)),
        file_size: file_size as i64,
        version: param_version.map(String::from),
        source_url: param_source.map(String::from),
        action: "installed".to_string(),
        installed: true,
        previous_version: String::new(),
        platform: String::new(),
    })
}

/// Detect file type by magic bytes.
/// Returns one of: "elf" | "pe" | "script" | "tar.gz" | "zip" | "executable"
///
/// **Note**: only `"tar.gz"` and `"zip"` are accepted by the installer;
/// other types cause [`AgentMgmtError::UnsupportedType`].
///
/// **Limitation**: any gzip file (magic `1F 8B`) is classified as `"tar.gz"`.
/// Plain `.gz` files will fail at the `tar::Archive::entries()` stage with
/// a tar-specific error message.
pub fn detect_file_type(bytes: &[u8]) -> String {
    if bytes.len() >= 4 {
        // ELF: 7F 45 4C 46
        if &bytes[0..4] == b"\x7FELF" {
            return "elf".into();
        }
        // PE / MZ
        if &bytes[0..2] == b"MZ" {
            return "pe".into();
        }
        // gzip: 1F 8B
        if &bytes[0..2] == b"\x1F\x8B" {
            return "tar.gz".into();
        }
        // zip: 50 4B 03 04
        if &bytes[0..4] == b"PK\x03\x04" {
            return "zip".into();
        }
    }
    if bytes.starts_with(b"#!") {
        return "script".into();
    }
    "executable".into()
}

#[allow(dead_code)]
fn make_executable_sync(path: &Path) -> AgentMgmtResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(path)?;
        let mut perms = metadata.permissions();
        perms.set_mode(perms.mode() | 0o755);
        std::fs::set_permissions(path, perms)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
                bytes: Bytes::from_static(payload),
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
                bytes: Bytes::from(payload),
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
                bytes: Bytes::from(payload),
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
                bytes: Bytes::from(payload),
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

        let resp = install_from_prepared_stream(&r, &pm, metadata, Bytes::new(), s)
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
        let first_data = Bytes::from(archive[..mid].to_vec());
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

        let err = install_from_prepared_stream(&r, &pm, metadata, Bytes::new(), s)
            .await
            .unwrap_err();
        assert!(matches!(err, AgentMgmtError::StreamTruncated));
    }
}
