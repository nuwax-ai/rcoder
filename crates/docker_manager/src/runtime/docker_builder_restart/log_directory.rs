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

/// 替代容器拿到的日志目录（宿主机路径），记在它的 label 上
pub(super) const RESTART_LOG_DIR: &str = "rcoder.io/restart-log-dir";
const CONTAINER_LOGS_DIR_ENV: &str = "CONTAINER_LOGS_DIR";
const CONTAINER_LOG_NAME_ENV: &str = "CONTAINER_LOG_NAME";

impl Archive {
    /// 容器的日志挂载点：`apply_auto_mounts` 给容器内路径带 `container-logs` 的那条挂载写的
    pub(super) fn log_target(&self) -> Option<&str> {
        self.env
            .iter()
            .find_map(|entry| entry.strip_prefix(&format!("{CONTAINER_LOGS_DIR_ENV}=")))
    }
}

/// 原容器的日志挂载，以及替代容器的日志目录放在哪
pub(super) struct LogDirectoryPlan<'a> {
    /// 日志挂载在归档模板里的见证：`name` 是挂载点，`uid` 是不能再绑一次的旧目录
    pub(super) volume: &'a AppResourceIdentity,
    /// 这条挂载在 `Archive::binds` 里的下标
    pub(super) bind: usize,
    /// 旧目录的上级目录（宿主机路径）
    pub(super) parent: PathBuf,
    /// 新目录的名字；已被占用时换别的
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
            // CONTAINER_LOGS_DIR 与任何一条挂载都对不上（旧版本把带变量的容器路径原样写了进去）。
            // 认不出日志挂载就换不了目录，只能照归档重放
            tracing::warn!(
                target,
                "Restart log mount target does not match any archived bind; replaying the archived log directory as it is"
            );
            return Ok(None);
        };
        // 按见证（源加挂载点）整段前缀去找那条 bind，不按冒号切分：源路径里有 Windows 盘符时
        // 切分有歧义。
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

/// 替代容器绑定的日志目录
pub(super) struct LogDirectory {
    /// 日志目录在容器内的挂载点
    pub(super) target: String,
    /// 新目录在宿主机上的路径
    pub(super) source: String,
    /// RCoder 亲手建出来的那个目录（RCoder 进程里的路径）；改由 Docker 建时为 `None`
    pub(super) created: Option<PathBuf>,
}

/// 替代容器应当有的挂载：模板里的卷，其中 `target` 上的日志挂载指向 `source`。
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

/// 把重放用的归档指向替代容器的新日志目录。这条 bind 的其它部分（挂载点、选项）和别的
/// bind 都不动。
pub(super) fn rebind_log_directory(
    replay: &mut Archive,
    plan: &LogDirectoryPlan<'_>,
    log: &LogDirectory,
) {
    let archived = format!("{}:{}", plan.volume.uid, plan.volume.name);
    let Some(options) = replay.binds[plan.bind].strip_prefix(&archived) else {
        // `plan` 只可能来自同一份归档，前缀在那里核对过
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
    /// 在归档里的日志目录旁边，给替代容器新建一个。
    ///
    /// 建目录和创建容器时一样尽力而为：RCoder 建不出来，就由 Docker 随容器一起建。名字全被
    /// 占是另一回事——绑定任何已有目录正是这里不能做的。
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
        // 名字可能换过：被占用的名字不复用
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

/// 替代容器没建成：刚为它建的目录没人用。只删空目录，有东西写进去的不删。
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
