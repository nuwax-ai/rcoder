//! 替代容器的换源记录：写在它的 label `rcoder.io/restart-log-dir` 上（挂载点 → 新目录的 JSON）。
//!
//! 这份记录是对"这一个替代容器"写下的，随它存在。续做时按它核对，引擎也按它放行：两边读的是
//! 同一份。不在两个时点各算一遍——算出来的那份会变（挂载表时有时无、日志根的配置改过），
//! 记在容器上的那份不会。
//!
//! 读的时候不重新定计划，只按记录自己核。每一项都得是：
//!
//! - 归档里的一条 HostPath 挂载；
//! - 不是持久数据（`/home/user` 下面的挂载永不换源）；
//! - 旧目录的同级新目录：绝对路径、同一个上级、不是旧目录本身；
//! - 不是"确定既不在可回收日志根里、又不是日志挂载"。判不出来不拦：这份记录是我们自己写在
//!   带归档标记的替代容器上的，替代容器也已经绑着它记的目录；挂载表一时取不到就拒绝，续做
//!   会一直卡在恢复里。
//!
//! 没有这份记录的替代容器是旧版本建的，那时归档原样重放，它绑的就是归档里的目录。

use std::collections::HashMap;

use bollard::models::ContainerInspectResponse;
use container_runtime_api::ContainerRuntimeResult as Result;
use shared_types::{
    AppResourceIdentity, AppResourceKind, BuilderRestartMountWitness, BuilderRestartTemplate,
    ReplacedBuilderMount,
};

use super::log_directory::{is_log_mount, with_log_sources};
use super::{Archive, DockerRuntime, RESTORE, conflict, inspect_absent};
use crate::runtime::docker_runtime::host_path_space::{LogDomainAdmission, LogDomainVerdict};

/// 替代容器拿到的日志目录（挂载点 → 宿主机路径的 JSON），记在它的 label 上
pub(super) const RESTART_LOG_DIR: &str = "rcoder.io/restart-log-dir";

/// 上一次中断前建好的替代容器
pub(super) struct ResumedReplacement {
    /// 替代容器的 Docker ID。最终核验必须是同一个容器
    pub(super) id: String,
    /// 它记在 label 上的日志目录：挂载点 → 新目录
    log_sources: HashMap<String, String>,
}

impl ResumedReplacement {
    /// 它应当有的挂载：归档里的卷，其中换过源的挂载点换成新目录
    pub(super) fn expected_volumes(
        &self,
        template: &BuilderRestartTemplate,
    ) -> Vec<AppResourceIdentity> {
        with_log_sources(&template.volumes, &self.log_sources)
    }

    /// 它记下的换源见证：每一项配上归档里的旧目录
    fn witness(&self, template: &BuilderRestartTemplate) -> BuilderRestartMountWitness {
        let replaced = self
            .log_sources
            .iter()
            .map(|(target, source)| ReplacedBuilderMount {
                target: target.clone(),
                // resumed_sources 核过：每个挂载点都是归档里的 HostPath 卷
                previous_source: template
                    .volumes
                    .iter()
                    .find(|volume| volume.name == *target)
                    .map(|volume| volume.uid.clone())
                    .unwrap_or_default(),
                source: source.clone(),
            })
            .collect();
        BuilderRestartMountWitness {
            container_id: self.id.clone(),
            replaced,
        }
    }
}

impl DockerRuntime {
    /// 按名称认下这次恢复的替代容器：只接受带着这次归档标记、没在运行的。它用的日志目录从
    /// 它自己的 label 读
    pub(super) async fn resumed_replacement(
        &self,
        template: &BuilderRestartTemplate,
        archive: &Archive,
        admission: &LogDomainAdmission<'_>,
        existing: ContainerInspectResponse,
    ) -> Result<ResumedReplacement> {
        let labels = existing
            .config
            .as_ref()
            .and_then(|config| config.labels.clone())
            .unwrap_or_default();
        let id = existing.id.clone().filter(|id| !id.is_empty());
        if labels.get(RESTORE) != Some(&template.archive.uid)
            || existing.state.as_ref().and_then(|state| state.running) == Some(true)
        {
            return Err(conflict("Another builder occupies restart target"));
        }
        let id = id.ok_or_else(|| conflict("Restart target has no physical ID"))?;
        // 没有换源记录的是旧版本建的，那时归档原样重放，它绑的就是归档里的目录
        let log_sources = match labels.get(RESTART_LOG_DIR) {
            None => HashMap::new(),
            Some(label) => resumed_sources(label, template, archive, admission).await?,
        };
        Ok(ResumedReplacement { id, log_sources })
    }

    /// 这次重启的替代容器自己记下的换源见证：按名称读它的 label，不重新算。替代容器不存在
    /// 时为 `None`
    pub(in crate::runtime) async fn restart_mount_witness(
        &self,
        template: &BuilderRestartTemplate,
    ) -> Result<Option<BuilderRestartMountWitness>> {
        let original = template
            .source
            .workload
            .as_ref()
            .filter(|resource| resource.kind == AppResourceKind::Container)
            .ok_or_else(|| conflict("Restart source container is missing"))?;
        let client = self.inner.get_docker_client();
        let Some(existing) = inspect_absent(
            client.inspect_container(&original.name, None).await,
            "Inspect restart target",
        )?
        else {
            return Ok(None);
        };
        let archive = self.load_archive(template).await?;
        let admission = self.log_domain().await;
        let resumed = self
            .resumed_replacement(template, &archive, &admission, existing)
            .await?;
        Ok(Some(resumed.witness(template)))
    }
}

/// 替代容器 label 里记的日志目录（挂载点 → 新目录），按记录自己核，见模块说明
async fn resumed_sources(
    label: &str,
    template: &BuilderRestartTemplate,
    archive: &Archive,
    admission: &LogDomainAdmission<'_>,
) -> Result<HashMap<String, String>> {
    let sources: HashMap<String, String> = serde_json::from_str(label).map_err(|error| {
        conflict(format!(
            "Restart target carries an unreadable log directory label: {error}"
        ))
    })?;
    for (name, source) in &sources {
        let volume = template
            .volumes
            .iter()
            .find(|volume| volume.kind == AppResourceKind::HostPath && volume.name == *name)
            .ok_or_else(|| {
                conflict(format!(
                    "Restart target's log directory label names mount {name}, which is not a bind of the archived builder"
                ))
            })?;
        if shared_types::is_builder_persistent_data_mount(name) {
            return Err(conflict(format!(
                "Restart target's log directory label names mount {name}, which holds persistent data and is never replaced"
            )));
        }
        ensure_fresh_sibling(volume, source)?;
        match admission.classify(name, &volume.uid).await {
            LogDomainVerdict::Inside | LogDomainVerdict::Unknown(_) => {}
            LogDomainVerdict::Outside if is_log_mount(name, archive) => {}
            LogDomainVerdict::Outside => {
                return Err(conflict(format!(
                    "Restart target's log directory label names mount {name}, which is not a log mount"
                )));
            }
        }
    }
    Ok(sources)
}

/// `source` 得是 `volume` 原来绑定的目录的同级新目录
fn ensure_fresh_sibling(volume: &AppResourceIdentity, source: &str) -> Result<()> {
    let previous = std::path::Path::new(&volume.uid);
    let path = std::path::Path::new(source);
    if !path.is_absolute() || path.parent() != previous.parent() || source == volume.uid {
        return Err(conflict(format!(
            "Restart target records an unexpected log directory for mount {}",
            volume.name
        )));
    }
    Ok(())
}
