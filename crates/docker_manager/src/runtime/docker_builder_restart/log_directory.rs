//! 替代 builder 的日志目录。
//!
//! 归档重放时只有日志挂载不照原样。原容器绑定过的那个目录是一次性的：builder 停着的这段
//! 时间里它可能已经过期，日志清理器随时在回收。替代容器换用同级的新目录（见
//! `crate::log_directory`），`CONTAINER_LOG_NAME` 跟着换；新目录记在容器的 label 里，中断后
//! 续做的恢复据此核对。

use container_runtime_api::ContainerRuntimeResult as Result;
use shared_types::{AppResourceIdentity, BuilderRestartTemplate};
use std::path::PathBuf;

use super::{Archive, DockerRuntime, conflict};
use crate::runtime::docker_runtime::host_path_space::{
    CreateHostDirectoryError, create_host_directory,
};

/// The log directory given to a replacement by restart restore.
pub(super) const RESTART_LOG_DIR: &str = "rcoder.io/restart-log-dir";
const CONTAINER_LOGS_DIR_ENV: &str = "CONTAINER_LOGS_DIR";
const CONTAINER_LOG_NAME_ENV: &str = "CONTAINER_LOG_NAME";

impl Archive {
    /// Where the container keeps its logs: set by `apply_auto_mounts` for the
    /// mount whose container path carries `container-logs`
    pub(super) fn log_target(&self) -> Option<&str> {
        self.env
            .iter()
            .find_map(|entry| entry.strip_prefix(&format!("{CONTAINER_LOGS_DIR_ENV}=")))
    }
}

/// The original's log mount, and where the replacement's log directory goes
pub(super) struct LogDirectoryPlan<'a> {
    /// Archived mount witness of the log mount: `name` is the target, `uid` the
    /// directory that must not be bound again
    pub(super) volume: &'a AppResourceIdentity,
    /// Index of that mount's entry in `Archive::binds`
    pub(super) bind: usize,
    /// Parent of the old directory, on the host
    pub(super) parent: PathBuf,
    /// Name the new directory gets unless it is already taken
    pub(super) preferred: String,
}

impl LogDirectoryPlan<'_> {
    pub(super) fn plan<'a>(
        template: &'a BuilderRestartTemplate,
        archive: &Archive,
    ) -> Result<Option<LogDirectoryPlan<'a>>> {
        let Some(target) = archive.log_target() else {
            return Ok(None);
        };
        let Some(volume) = template.volumes.iter().find(|volume| volume.name == target) else {
            return Ok(None);
        };
        // Match the bind by the witness (source and target) rather than by
        // splitting on colons: a Windows drive letter in the source makes that
        // ambiguous.
        let archived = format!("{}:{}", volume.uid, volume.name);
        let bind = archive
            .binds
            .iter()
            .position(|bind| bind == &archived || bind.starts_with(&format!("{archived}:")))
            .ok_or_else(|| {
                conflict(format!(
                    "Restart log bind does not match the archived mount witness for {target}"
                ))
            })?;
        let previous = std::path::Path::new(&volume.uid);
        let (Some(parent), Some(name)) = (
            previous.parent(),
            previous.file_name().and_then(|name| name.to_str()),
        ) else {
            return Err(conflict(format!(
                "Restart log bind source is not a directory path: {}",
                volume.uid
            )));
        };
        Ok(Some(LogDirectoryPlan {
            volume,
            bind,
            parent: parent.to_path_buf(),
            preferred: crate::log_directory::replacement_log_directory_name(
                name,
                chrono::Utc::now(),
            ),
        }))
    }
}

/// The log directory the replacement binds
pub(super) struct LogDirectory {
    /// Container mount target of the log directory
    pub(super) target: String,
    /// The new directory on the host
    pub(super) source: String,
    /// The directory as RCoder created it. `None` when Docker has to create it
    pub(super) created: Option<PathBuf>,
}

/// The mounts the replacement must have: the template's volumes, with the log
/// mount at `target` pointing at `source`.
pub(super) fn volumes_with_log_source(
    volumes: &[AppResourceIdentity],
    target: &str,
    source: &str,
) -> Vec<AppResourceIdentity> {
    volumes
        .iter()
        .map(|volume| {
            if volume.name == target {
                AppResourceIdentity {
                    uid: source.to_owned(),
                    ..volume.clone()
                }
            } else {
                volume.clone()
            }
        })
        .collect()
}

/// Point the replayed archive at the replacement's new log directory. Everything
/// else about that bind — target, options — and every other bind is left alone.
pub(super) fn rebind_log_directory(
    replay: &mut Archive,
    plan: &LogDirectoryPlan<'_>,
    log: &LogDirectory,
) {
    let archived = format!("{}:{}", plan.volume.uid, plan.volume.name);
    let Some(options) = replay.binds[plan.bind].strip_prefix(&archived) else {
        // `plan` only comes from this same archive, where the prefix was verified
        return;
    };
    replay.binds[plan.bind] = format!("{}:{}{}", log.source, plan.volume.name, options);
    if let Some(name) = std::path::Path::new(&log.source).file_name() {
        let name = name.to_string_lossy();
        let variable = format!("{CONTAINER_LOG_NAME_ENV}=");
        for entry in &mut replay.env {
            if entry.starts_with(&variable) {
                *entry = format!("{variable}{name}");
            }
        }
    }
}

impl DockerRuntime {
    /// Give the replacement a new log directory next to the archived one.
    ///
    /// Creating it is best effort, as it is at container creation: when RCoder cannot
    /// create it, Docker creates it with the container. Every name being taken is
    /// different — binding any existing directory is exactly what must not happen.
    pub(super) async fn create_log_directory(
        &self,
        plan: &LogDirectoryPlan<'_>,
    ) -> Result<LogDirectory> {
        let target = plan.volume.name.clone();
        let preferred = plan.parent.join(&plan.preferred);
        let created = match create_host_directory(&self.inner, &self.log_path_resolver, &preferred)
            .await
        {
            Ok(created) => created,
            Err(
                error @ CreateHostDirectoryError::Create(
                    crate::log_directory::FreshDirectoryError::Exhausted,
                ),
            ) => {
                return Err(conflict(format!(
                    "Cannot create a new log directory for the replacement builder: {error}"
                )));
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    target = %target,
                    "Cannot create a new log directory for the replacement builder from inside RCoder; Docker creates it with the container"
                );
                return Ok(LogDirectory {
                    target,
                    source: preferred.to_string_lossy().into_owned(),
                    created: None,
                });
            }
        };
        // The name may have changed: a taken name is never reused
        let name = created
            .file_name()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&plan.preferred));
        Ok(LogDirectory {
            target,
            source: plan.parent.join(name).to_string_lossy().into_owned(),
            created: Some(created),
        })
    }
}

/// The replacement was not created: the directory just made for it has no user.
/// Only an empty directory is removed, never one something has written to.
pub(super) async fn discard_log_directory(created: &std::path::Path) {
    if let Err(error) = tokio::fs::remove_dir(created).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            directory = %created.display(),
            %error,
            "Cannot remove the unused log directory of a builder that was not created"
        );
    }
}
