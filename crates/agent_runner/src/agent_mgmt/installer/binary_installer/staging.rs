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

use shared_types::InstallType;
use shared_types_grpc::InstallAgentResponse;
use tracing::{debug, info, warn};

use crate::agent_mgmt::error::{AgentMgmtError, AgentMgmtResult};
use crate::agent_mgmt::installer::AgentManifest;
use crate::agent_mgmt::installer::archive_installer;
use crate::agent_mgmt::registry::AgentRegistry;

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
        Ok(Err(mut e)) => {
            cleanup_failed_install(agent_dir, registry.install_dir(), &mut e).await;
            return Err(e);
        }
        Err(mut outer) => {
            cleanup_failed_install(agent_dir, registry.install_dir(), &mut outer).await;
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
    registry.upsert(manifest).await?;

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

/// 回滚未完成时目录内仍有待恢复的数据，不能按普通安装失败清理。
async fn cleanup_failed_install(
    agent_dir: &Path,
    recovery_root: &Path,
    error: &mut AgentMgmtError,
) {
    match error {
        AgentMgmtError::NormalizeRollbackIncomplete(source) => {
            preserve_failed_normalization(agent_dir, recovery_root, source).await;
            warn!(path = %agent_dir.display(), %source, "preserving incomplete archive rollback");
        }
        AgentMgmtError::NotFound(_)
        | AgentMgmtError::InvalidManifest(_)
        | AgentMgmtError::ChecksumMismatch { .. }
        | AgentMgmtError::ArchiveBomb { .. }
        | AgentMgmtError::PathTraversal(_)
        | AgentMgmtError::CommandTimeout(_)
        | AgentMgmtError::InstallFailed(_)
        | AgentMgmtError::BinaryTooLarge { .. }
        | AgentMgmtError::BuiltinProtected
        | AgentMgmtError::StreamTruncated
        | AgentMgmtError::InvalidChunk(_)
        | AgentMgmtError::PlatformNotFound(_)
        | AgentMgmtError::InvalidVersion(_)
        | AgentMgmtError::InstallCancelled
        | AgentMgmtError::VersionAlreadyInstalled { .. }
        | AgentMgmtError::Io(_)
        | AgentMgmtError::Archive(_)
        | AgentMgmtError::Json(_)
        | AgentMgmtError::UnsupportedType(_) => {
            if let Err(cleanup_error) = tokio::fs::remove_dir_all(agent_dir).await {
                warn!(path = %agent_dir.display(), %cleanup_error, "failed to clean up incomplete agent install");
            }
        }
    }
}

/// 仅安装器拥有完整的本轮解压目录，可把失败树隔离到安装根目录下的唯一恢复目录。
/// 恢复目录与 agent_id 同级，后续有版本/无版本安装均不会清理旧恢复数据。
/// 通用 archive 归一化也服务用户工作区，不能在那里移动整个 target。
async fn preserve_failed_normalization(
    agent_dir: &Path,
    recovery_root: &Path,
    error: &mut download_utils::ArchiveError,
) {
    use download_utils::ArchiveError;
    let (rollback, residual) = match error {
        ArchiveError::NormalizeRollbackIncomplete {
            rollback, residual, ..
        } => (rollback, residual),
        ArchiveError::Io(_)
        | ArchiveError::PathTraversal(_)
        | ArchiveError::InvalidArchive(_)
        | ArchiveError::TooLarge { .. } => return,
    };
    let recovery_dir = recovery_root.join(format!(".archive-recovery-{}", uuid::Uuid::new_v4()));
    if let Err(error) = tokio::fs::create_dir(&recovery_dir).await {
        rollback.push((
            agent_dir.to_path_buf(),
            format!(
                "create recovery directory {}: {error}; rollback data left in place",
                recovery_dir.display()
            ),
        ));
        add_residual(residual, agent_dir.to_path_buf());
        return;
    }

    // agent_dir 内的残留随整树一起移动；外部残留是归一化自己移出的 wrapper。
    let mut sources = vec![agent_dir.to_path_buf()];
    for path in residual.iter() {
        if !path.starts_with(agent_dir) && !sources.contains(path) {
            sources.push(path.clone());
        }
    }
    let mut preserved = false;
    for (index, source) in sources.into_iter().enumerate() {
        match tokio::fs::symlink_metadata(&source).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                rollback.push((
                    source.clone(),
                    format!("inspect rollback data before recovery: {error}; data left in place"),
                ));
                add_residual(residual, source);
                continue;
            }
        }
        let destination = recovery_dir.join(if index == 0 {
            "extracted".to_string()
        } else {
            format!("wrapper-{index}")
        });
        if let Err(error) = tokio::fs::rename(&source, &destination).await {
            rollback.push((
                source.clone(),
                format!(
                    "preserve rollback data at {}: {error}; data left in place",
                    destination.display()
                ),
            ));
            add_residual(residual, source);
            continue;
        }
        preserved = true;
        for (path, _) in rollback.iter_mut() {
            relocate_diagnostic_path(path, &source, &destination);
        }
        for path in residual.iter_mut() {
            relocate_diagnostic_path(path, &source, &destination);
        }
        add_residual(residual, destination.clone());
        warn!(from = %source.display(), to = %destination.display(), "preserved incomplete archive rollback data");
    }
    if !preserved && let Err(error) = tokio::fs::remove_dir(&recovery_dir).await {
        debug!(path = %recovery_dir.display(), %error, "failed to remove empty recovery directory");
    }
}

fn add_residual(residual: &mut Vec<std::path::PathBuf>, path: std::path::PathBuf) {
    if !residual.contains(&path) {
        residual.push(path);
    }
}

fn relocate_diagnostic_path(path: &mut std::path::PathBuf, source: &Path, destination: &Path) {
    if let Ok(relative) = path.strip_prefix(source) {
        *path = destination.join(relative);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn incomplete_rollback_cleanup_preserves_data_and_allows_install_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let pm =
            crate::agent_mgmt::path_manager::PathManager::new_with_root(tmp.path().to_path_buf());
        let registry = AgentRegistry::empty(pm.clone());
        let agent_dir = pm.agent_version_dir("agent", "1.0.0").unwrap();
        let wrapper_tmp = agent_dir.parent().unwrap().join("1.0.0__wrapper_tmp.old");
        std::fs::create_dir_all(agent_dir.join("restored-wrapper")).unwrap();
        std::fs::create_dir_all(&wrapper_tmp).unwrap();
        let child = agent_dir.join("still-outside-wrapper.txt");
        std::fs::write(&child, "moved child").unwrap();
        std::fs::write(agent_dir.join("restored-wrapper/other.txt"), "other").unwrap();
        std::fs::write(wrapper_tmp.join("unmoved.txt"), "wrapper child").unwrap();
        let mut error =
            AgentMgmtError::from(download_utils::ArchiveError::NormalizeRollbackIncomplete {
                original: Box::new(download_utils::ArchiveError::Io(std::io::Error::other(
                    "move failed",
                ))),
                rollback: vec![(child.clone(), "move child back: denied".into())],
                residual: vec![child, wrapper_tmp.clone()],
            });

        cleanup_failed_install(&agent_dir, pm.install_dir(), &mut error).await;

        assert!(!agent_dir.exists());
        assert!(!wrapper_tmp.exists());
        let AgentMgmtError::NormalizeRollbackIncomplete(
            download_utils::ArchiveError::NormalizeRollbackIncomplete {
                rollback, residual, ..
            },
        ) = &error
        else {
            panic!("rollback error type must survive: {error:?}");
        };
        let preserved_child = &rollback[0].0;
        assert_eq!(
            std::fs::read_to_string(preserved_child).unwrap(),
            "moved child"
        );
        let recovery = preserved_child.parent().unwrap().parent().unwrap();
        assert_eq!(
            std::fs::read_to_string(recovery.join("extracted/restored-wrapper/other.txt")).unwrap(),
            "other"
        );
        assert_eq!(
            std::fs::read_to_string(recovery.join("wrapper-1/unmoved.txt")).unwrap(),
            "wrapper child"
        );
        assert!(residual.iter().all(|path| path.starts_with(recovery)));
        assert_eq!(
            error.error_code(),
            shared_types::error_codes::ERR_AGENT_MGMT_INSTALL_FAILED
        );
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("move failed"), "{diagnostic}");
        assert!(
            diagnostic.contains(&preserved_child.display().to_string()),
            "{diagnostic}"
        );

        // 从旧版本失败重走真正的无版本 bytes 安装入口，清理整个 agent_id 后
        // 重新解压、注册成功，恢复目录仍在 agent_id 外且数据保留。
        let script = b"#!/bin/sh\necho retry\n";
        let mut payload = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut payload, flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            let mut header = tar::Header::new_gnu();
            header.set_size(script.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, "agent", &script[..]).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let response = super::super::install_from_bytes(
            &registry,
            &pm,
            super::super::InstallBytesParams {
                agent_id: "agent",
                command: "agent",
                args: &[],
                expected_sha256: None,
                install_type: InstallType::Binary,
                bytes: ::bytes::Bytes::from(payload),
                version: None,
                source: None,
            },
        )
        .await
        .unwrap();
        assert!(response.installed);
        assert_eq!(
            std::fs::read(pm.agent_dir("agent").unwrap().join("agent")).unwrap(),
            script
        );
        assert_eq!(
            std::fs::read_to_string(preserved_child).unwrap(),
            "moved child"
        );
    }

    #[tokio::test]
    async fn recovery_creation_failure_keeps_original_data_and_reports_it() {
        let tmp = tempfile::tempdir().unwrap();
        let parent_file = tmp.path().join("not-a-directory");
        std::fs::write(&parent_file, "keep").unwrap();
        let agent_dir = parent_file.join("agent");
        let wrapper_tmp = tmp.path().join("agent__wrapper_tmp.old");
        std::fs::create_dir_all(&wrapper_tmp).unwrap();
        std::fs::write(wrapper_tmp.join("unmoved.txt"), "unmoved").unwrap();
        let mut error =
            AgentMgmtError::from(download_utils::ArchiveError::NormalizeRollbackIncomplete {
                original: Box::new(download_utils::ArchiveError::Io(std::io::Error::other(
                    "move failed",
                ))),
                rollback: vec![],
                residual: vec![wrapper_tmp.clone()],
            });

        cleanup_failed_install(&agent_dir, &parent_file, &mut error).await;

        assert_eq!(
            std::fs::read_to_string(wrapper_tmp.join("unmoved.txt")).unwrap(),
            "unmoved"
        );
        assert_eq!(std::fs::read_to_string(parent_file).unwrap(), "keep");
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("data left in place"), "{diagnostic}");
        assert!(
            diagnostic.contains(&wrapper_tmp.display().to_string()),
            "{diagnostic}"
        );
    }

    #[tokio::test]
    async fn ordinary_failed_install_cleanup_still_removes_partial_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("partial.txt"), "partial").unwrap();

        cleanup_failed_install(
            &agent_dir,
            tmp.path(),
            &mut AgentMgmtError::InstallFailed("missing entrypoint".into()),
        )
        .await;

        assert!(!agent_dir.exists());
    }
}
