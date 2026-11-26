//! 替代 builder 的日志目录。
//!
//! 归档重放时，落在可回收日志根下面的 bind 不照原样重放：原容器绑定过的目录可能已经过期，
//! 日志清理器随时在回收它。替代容器的每一条这样的 bind 都换成同级的新目录（独占新建，见
//! `crate::log_directory`），并登记为正在准备挂载。
//!
//! 按**源路径**识别，不看容器内的挂载点叫什么，也不看环境变量：旧版本的归档里环境变量可能是
//! 带变量的模板，一个容器也可以有几条这样的挂载。
//!
//! 新目录记在替代容器的 label `rcoder.io/restart-log-dir` 上（挂载点 → 新目录）：恢复中断后
//! 续做时据此核对。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use container_runtime_api::ContainerRuntimeResult as Result;
use shared_types::{AppResourceIdentity, AppResourceKind, BuilderRestartTemplate};

use super::{Archive, DockerRuntime, conflict};
use crate::log_directory::{LogDomain, PendingBinds};
use crate::runtime::docker_runtime::host_path_space::{create_host_directory, resolve_log_domain};

/// 替代容器拿到的日志目录（挂载点 → 宿主机路径的 JSON），记在它的 label 上
pub(super) const RESTART_LOG_DIR: &str = "rcoder.io/restart-log-dir";
const CONTAINER_LOGS_DIR_ENV: &str = "CONTAINER_LOGS_DIR";
const CONTAINER_LOG_NAME_ENV: &str = "CONTAINER_LOG_NAME";

impl DockerRuntime {
    /// 可回收的日志根在宿主机上的位置。
    ///
    /// 解析器起不来时恢复直接失败：不知道日志域在哪，就没法判断哪些 bind 不能原样重放。
    pub(super) async fn log_domain(&self) -> Result<LogDomain> {
        resolve_log_domain(
            &self.inner,
            &self.log_path_resolver,
            &self.inner.config.log_reclaim_root,
        )
        .await
    }
}

/// 一次恢复里要换新目录的一条日志挂载
pub(super) struct LogMountPlan<'a> {
    /// 这条挂载在归档模板里的见证：`name` 是挂载点，`uid` 是不能再绑一次的旧目录
    pub(super) volume: &'a AppResourceIdentity,
    /// 这条挂载在 `Archive::binds` 里的下标
    bind: usize,
    /// 旧目录的上级目录（宿主机路径）
    parent: PathBuf,
    /// 新目录的名字；已被占用时换别的
    preferred: String,
}

/// 找出归档里所有落在可回收日志根下面的 bind。
///
/// 源在日志根下、却没有挂载见证的 bind 改不了它，原样重放又可能绑到旧目录，遇到就拒绝。
pub(super) fn plan<'a>(
    template: &'a BuilderRestartTemplate,
    archive: &Archive,
    domain: &LogDomain,
) -> Result<Vec<LogMountPlan<'a>>> {
    let mut plans: Vec<LogMountPlan<'a>> = Vec::new();
    for volume in template
        .volumes
        .iter()
        .filter(|volume| volume.kind == AppResourceKind::HostPath && domain.contains(&volume.uid))
    {
        // 按见证（源加挂载点）整段前缀去找那条 bind，不按冒号切分：源路径里有 Windows 盘符时
        // 切分有歧义。
        let archived = format!("{}:{}", volume.uid, volume.name);
        let bind = archive
            .binds
            .iter()
            .position(|bind| bind == &archived || bind.starts_with(&format!("{archived}:")))
            .ok_or_else(|| {
                conflict(format!(
                    "Restart log bind does not match the archived mount witness for {}",
                    volume.name
                ))
            })?;
        let previous = Path::new(&volume.uid);
        let (Some(parent), Some(previous_name)) = (
            previous.parent(),
            previous.file_name().and_then(|name| name.to_str()),
        ) else {
            return Err(conflict(format!(
                "Restart log bind source is not a directory path: {}",
                volume.uid
            )));
        };
        plans.push(LogMountPlan {
            volume,
            bind,
            parent: parent.to_path_buf(),
            preferred: crate::log_directory::replacement_log_directory_name(
                previous_name,
                chrono::Utc::now(),
            ),
        });
    }
    let stray = archive.binds.iter().enumerate().find_map(|(index, bind)| {
        (domain.contains_bind_source(bind) && !plans.iter().any(|plan| plan.bind == index))
            .then_some(bind)
    });
    if let Some(stray) = stray {
        return Err(conflict(format!(
            "Restart bind {stray} lies under the reclaimable log root but has no mount witness"
        )));
    }
    Ok(plans)
}

/// 这次恢复给替代容器新建的日志目录
pub(super) struct FreshLogDirectories {
    /// 挂载点 → 新目录（宿主机路径）
    sources: HashMap<String, String>,
    /// RCoder 进程里建出来的那几个目录
    created: Vec<PathBuf>,
    /// Docker 应答之前一直登记着，清理器不会把它们当成没人用
    pending: PendingBinds,
}

impl FreshLogDirectories {
    /// 逐条独占新建。任何一条建不出来都报错，并把已经建好的删掉——替代容器还没开始建，这些
    /// 目录没人用。不退回去绑定归档里的旧目录。
    pub(super) async fn create(
        runtime: &DockerRuntime,
        plans: &[LogMountPlan<'_>],
    ) -> Result<Self> {
        let mut fresh = Self {
            sources: HashMap::new(),
            created: Vec::new(),
            pending: PendingBinds::default(),
        };
        for plan in plans {
            let preferred = plan.parent.join(&plan.preferred);
            let created = match create_host_directory(
                &runtime.inner,
                &runtime.log_path_resolver,
                &preferred,
            )
            .await
            {
                Ok(created) => created,
                Err(error) => {
                    fresh.discard_created().await;
                    return Err(conflict(format!(
                        "Cannot create a new log directory for the replacement builder, mount {}: {error}",
                        plan.volume.name
                    )));
                }
            };
            // 名字可能换过：被占用的名字不复用
            let name = created
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&plan.preferred));
            let source = plan.parent.join(name).to_string_lossy().into_owned();
            fresh.pending.claim(Path::new(&source));
            fresh.created.push(created);
            fresh.sources.insert(plan.volume.name.clone(), source);
        }
        Ok(fresh)
    }

    /// 改写重放用的归档：这些 bind 指向新目录，挂载点和选项不动
    pub(super) fn apply(&self, replay: &mut Archive, plans: &[LogMountPlan<'_>]) {
        let named_target = replay.log_target().map(str::to_owned);
        for plan in plans {
            let Some(source) = self.sources.get(&plan.volume.name) else {
                continue;
            };
            let archived = format!("{}:{}", plan.volume.uid, plan.volume.name);
            if let Some(options) = replay.binds[plan.bind].strip_prefix(&archived) {
                replay.binds[plan.bind] = format!("{source}:{}{}", plan.volume.name, options);
            }
            // 环境变量只描述其中一条日志挂载：挂载点对得上的那条才改
            if named_target.as_deref() != Some(plan.volume.name.as_str()) {
                continue;
            }
            if let Some(name) = Path::new(source).file_name() {
                let variable = format!("{CONTAINER_LOG_NAME_ENV}=");
                for entry in &mut replay.env {
                    if entry.starts_with(&variable) {
                        *entry = format!("{variable}{}", name.to_string_lossy());
                    }
                }
            }
        }
    }

    /// 替代容器应当有的挂载
    pub(super) fn expected_volumes(
        &self,
        volumes: &[AppResourceIdentity],
    ) -> Vec<AppResourceIdentity> {
        with_log_sources(volumes, &self.sources)
    }

    /// 记到替代容器 label 上的值：挂载点 → 新目录
    pub(super) fn label(&self) -> String {
        // 里头只有字符串，序列化不会失败
        serde_json::to_string(&self.sources).unwrap_or_default()
    }

    /// 替代容器确认没有建成：删掉刚建的目录。只删得掉空目录，有东西写进去的不删
    pub(super) async fn discard_created(&self) {
        for created in &self.created {
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
    }
}

impl Archive {
    /// 容器的日志挂载点：`apply_auto_mounts` 给容器内路径带 `container-logs` 的那条挂载写的
    fn log_target(&self) -> Option<&str> {
        self.env
            .iter()
            .find_map(|entry| entry.strip_prefix(&format!("{CONTAINER_LOGS_DIR_ENV}=")))
    }
}

/// 续做时替代容器 label 里记的日志目录。每条日志挂载都要有，而且新目录得像模像样：绝对
/// 路径、和旧目录同级、不是旧目录。
pub(super) fn resumed_sources(
    label: &str,
    plans: &[LogMountPlan<'_>],
) -> Result<HashMap<String, String>> {
    let sources: HashMap<String, String> = serde_json::from_str(label).map_err(|error| {
        conflict(format!(
            "Restart target carries an unreadable log directory label: {error}"
        ))
    })?;
    if plans.is_empty() {
        return Err(conflict(
            "Restart target carries a log directory label but has no mount under the reclaimable log root",
        ));
    }
    for plan in plans {
        let Some(source) = sources.get(&plan.volume.name) else {
            return Err(conflict(format!(
                "Restart target records no log directory for mount {}",
                plan.volume.name
            )));
        };
        let path = Path::new(source);
        if !path.is_absolute()
            || path.parent() != Some(plan.parent.as_path())
            || source == &plan.volume.uid
        {
            return Err(conflict(format!(
                "Restart target records an unexpected log directory for mount {}",
                plan.volume.name
            )));
        }
    }
    Ok(sources)
}

/// 模板里的卷，其中 `sources` 里有的挂载点换成对应的新目录
pub(super) fn with_log_sources(
    volumes: &[AppResourceIdentity],
    sources: &HashMap<String, String>,
) -> Vec<AppResourceIdentity> {
    volumes
        .iter()
        .map(|volume| match sources.get(&volume.name) {
            Some(source) => AppResourceIdentity {
                uid: source.clone(),
                ..volume.clone()
            },
            None => volume.clone(),
        })
        .collect()
}
