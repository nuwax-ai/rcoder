//! 共享安装核心与文件类型探测。

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
use sha2::Digest;
use shared_types::InstallType;
use shared_types_grpc::{InstallAgentRequest, InstallAgentResponse};
use tracing::{debug, info, warn};

use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};
use crate::agent_mgmt::installer::AgentManifest;
use crate::agent_mgmt::installer::archive_installer;
use crate::agent_mgmt::registry::AgentRegistry;

/// Server-side stream type (matches `tonic::Streaming<InstallAgentRequest>`).
pub type IncomingStream =
    Pin<Box<dyn Stream<Item = Result<InstallAgentRequest, tonic::Status>> + Send>>;

/// staging 安装参数
pub(super) struct StagingInstallParams<'a> {
    pub(super) agent_id: &'a str,
    pub(super) command: &'a str,
    pub(super) args: &'a [String],
    pub(super) install_type: InstallType,
    pub(super) file_type: String,
    pub(super) file_size: u64,
    pub(super) version: Option<&'a str>,
    pub(super) source: Option<&'a str>,
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
pub(super) async fn _install_from_staging(
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
        if let Err(e) = std::fs::remove_file(&staging_clone) {
            warn!(
                "[agent_mgmt] failed to remove staging archive after extraction: path={}, error={}",
                staging_clone.display(),
                e
            );
        }

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
pub(super) fn make_executable_sync(path: &Path) -> AgentMgmtResult<()> {
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
