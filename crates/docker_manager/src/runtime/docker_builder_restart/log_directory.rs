//! 替代 builder 的日志目录。
//!
//! 归档重放时，落在可回收日志根下面的 bind 不照原样重放：原容器绑定过的目录可能已经过期，
//! 日志清理器随时在回收它。替代容器的每一条这样的 bind 都换成同级的新目录（独占新建，见
//! `crate::log_directory`），并登记为正在准备挂载。
//!
//! 按**源路径**识别，不看容器内的挂载点叫什么，也不看环境变量：旧版本的归档里环境变量可能是
//! 带变量的模板，一个容器也可以有几条这样的挂载。在不在日志根下面由
//! `host_path_space::LogDomain` 判定，用的是清理器那个路径空间。
//!
//! 新目录记在替代容器的 label `rcoder.io/restart-log-dir` 上（挂载点 → 新目录的 JSON）：恢复
//! 中断后续做时据此核对。
//!
//! 判不出一条 bind 在不在日志根里时（日志根定位不完整、这条源的真实位置求不出来、取不到路径
//! 空间）不让恢复失败：这条 bind 照归档原样重放，它的源登记到替代容器建出来。清理器对它同样
//! 判不出来，而它确认不了就不删。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use container_runtime_api::ContainerRuntimeResult as Result;
use shared_types::{AppResourceIdentity, AppResourceKind, BuilderRestartTemplate};

use super::{Archive, DockerRuntime, conflict};
use crate::log_directory::PendingBinds;
use crate::runtime::docker_runtime::host_path_space::{
    HostPathSpace, LogDomainAdmission, LogDomainVerdict, bind_source, create_host_directory,
};

/// 替代容器拿到的日志目录（挂载点 → 宿主机路径的 JSON），记在它的 label 上
pub(super) const RESTART_LOG_DIR: &str = "rcoder.io/restart-log-dir";
const CONTAINER_LOGS_DIR_ENV: &str = "CONTAINER_LOGS_DIR";
const CONTAINER_LOG_NAME_ENV: &str = "CONTAINER_LOG_NAME";

impl DockerRuntime {
    /// 这次恢复用的路径空间和日志域。定位不完整也不让恢复失败，见 `LogDomainAdmission`
    pub(super) async fn log_domain(&self) -> LogDomainAdmission<'_> {
        LogDomainAdmission::locate(
            &self.inner,
            &self.log_path_resolver,
            &self.inner.config.log_reclaim_root,
        )
        .await
    }

    /// 重启会更换 bind 源的挂载（容器内路径）：源落在可回收日志根下面的那几条。
    ///
    /// 引擎重启之后重新采集挂载、和重启前比较时，只放行这几条换源，其余挂载严格核对。判不
    /// 出来的不在其中：它照归档原样重放，源不变。
    pub(in crate::runtime) async fn restart_replaced_mounts(
        &self,
        template: &BuilderRestartTemplate,
    ) -> Result<Vec<String>> {
        let admission = self.log_domain().await;
        let mut replaced = Vec::new();
        for volume in template
            .volumes
            .iter()
            .filter(|volume| volume.kind == AppResourceKind::HostPath)
        {
            let verdict = admission
                .classify(&format!("Mount {}", volume.name), &volume.uid)
                .await;
            if verdict == LogDomainVerdict::Inside {
                replaced.push(volume.name.clone());
            }
        }
        Ok(replaced)
    }
}

/// 一次恢复里要换新目录的一条日志挂载
pub(super) struct LogMountPlan<'a> {
    /// 这条挂载在归档模板里的见证：`name` 是挂载点，`uid` 是不能再绑一次的旧目录
    volume: &'a AppResourceIdentity,
    /// 这条挂载在 `Archive::binds` 里的下标
    bind: usize,
    /// 旧目录的上级目录（宿主机路径）
    parent: PathBuf,
    /// 新目录的名字；已被占用时换别的
    preferred: String,
}

impl LogMountPlan<'_> {
    pub(super) fn volume(&self) -> &AppResourceIdentity {
        self.volume
    }
}

/// 归档里落在可回收日志根下面的 bind 怎么处置
pub(super) struct LogBinds<'a> {
    /// 要换成新目录的，每条一个换源计划
    pub(super) plans: Vec<LogMountPlan<'a>>,
    /// 判不出在不在日志根里的 bind 的源：照归档原样重放，登记到替代容器建出来
    pub(super) unclassified: Vec<PathBuf>,
}

/// 找出归档里所有落在可回收日志根下面的 bind，每条对应一个换源计划；判不出在不在的另记
/// 它们的源。
///
/// 源在日志根下、却没有挂载见证（或者见证和 bind 对不上）的 bind 改不了它，原样重放又可能
/// 绑到旧目录，遇到就拒绝。
pub(super) async fn plan<'a>(
    template: &'a BuilderRestartTemplate,
    archive: &Archive,
    admission: &LogDomainAdmission<'_>,
) -> Result<LogBinds<'a>> {
    let mut binds = LogBinds {
        plans: Vec::new(),
        unclassified: Vec::new(),
    };
    for volume in template
        .volumes
        .iter()
        .filter(|volume| volume.kind == AppResourceKind::HostPath)
    {
        let verdict = admission
            .classify(&format!("Mount {}", volume.name), &volume.uid)
            .await;
        if verdict != LogDomainVerdict::Inside {
            continue;
        }
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
        binds.plans.push(LogMountPlan {
            volume,
            bind,
            parent: parent.to_path_buf(),
            preferred: crate::log_directory::replacement_log_directory_name(
                previous_name,
                chrono::Utc::now(),
            ),
        });
    }
    for (index, bind) in archive.binds.iter().enumerate() {
        if binds.plans.iter().any(|plan| plan.bind == index) {
            continue;
        }
        // 命名卷不是宿主机路径，落不到日志根下
        let Some(source) = bind_source(bind) else {
            continue;
        };
        match admission.classify(bind, source).await {
            LogDomainVerdict::Inside => {
                return Err(conflict(format!(
                    "Restart bind {bind} lies under the reclaimable log root but has no mount witness"
                )));
            }
            LogDomainVerdict::Outside => {}
            LogDomainVerdict::Unknown(_) => binds.unclassified.push(PathBuf::from(source)),
        }
    }
    Ok(binds)
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
    ///
    /// 判不出在不在日志根里的 bind 照原样重放，它们的源（`binds.unclassified`）一并登记。
    pub(super) async fn create(
        space: Option<&HostPathSpace<'_>>,
        binds: &LogBinds<'_>,
    ) -> Result<Self> {
        let mut fresh = Self {
            sources: HashMap::new(),
            created: Vec::new(),
            pending: PendingBinds::default(),
        };
        for source in &binds.unclassified {
            fresh.pending.claim(source);
        }
        for plan in &binds.plans {
            // 有换源计划就有路径空间：计划是靠它判出来的
            let Some(space) = space else {
                return Err(conflict(
                    "Restart cannot create a new log directory without RCoder's mount table",
                ));
            };
            let preferred = plan.parent.join(&plan.preferred);
            let created = match create_host_directory(space, &preferred).await {
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

    /// 创建请求马上发出
    pub(super) fn request_sent(&mut self) {
        self.pending.request_sent();
    }

    /// 替代容器建出来了：它的挂载从现在起在清单里，登记可以注销
    pub(super) fn created(&mut self) {
        self.pending.settled();
    }

    /// 确认替代容器没有建成：登记照常注销，刚建的目录没人用，删掉。只删得掉空目录
    pub(super) async fn confirmed_absent(&mut self) {
        self.pending.settled();
        self.discard_created().await;
    }

    /// 创建结果不明：替代容器可能已经建出来、正绑着这些目录。目录和登记都原样留着——
    /// 下一次恢复按名称找到它、按它的 label 核对；确实没建成的，登记过期后由清理器回收
    pub(super) fn outcome_unknown(&mut self) {
        self.pending.outcome_unknown();
    }

    async fn discard_created(&self) {
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

/// 续做时替代容器 label 里记的日志目录（挂载点 → 新目录）。
///
/// 定得出换源计划时（`plans`）：label 的键必须和计划一模一样——每条日志挂载都要有，也不能多。
/// 多出来的键会改写计划之外的挂载（比如工作区）的期望值，那正是这份核验要拦的。
///
/// 定不出计划时（`blind`，取不到路径空间）：没有计划可比，只要求 label 的每一项都是归档里
/// 某条 HostPath 挂载的新目录。不让恢复因为取不到路径空间而失败。
///
/// 两种情况下新目录都得是旧目录的同级新目录：绝对路径、同一个上级、不是旧目录本身。
pub(super) fn resumed_sources(
    label: &str,
    template: &BuilderRestartTemplate,
    plans: &[LogMountPlan<'_>],
    blind: bool,
) -> Result<HashMap<String, String>> {
    let sources: HashMap<String, String> = serde_json::from_str(label).map_err(|error| {
        conflict(format!(
            "Restart target carries an unreadable log directory label: {error}"
        ))
    })?;
    if blind {
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
            ensure_fresh_sibling(volume, source)?;
        }
        return Ok(sources);
    }
    let unexpected = sources.keys().find(|key| {
        !plans
            .iter()
            .any(|plan| plan.volume().name.as_str() == key.as_str())
    });
    if let Some(unexpected) = unexpected {
        return Err(conflict(format!(
            "Restart target's log directory label names mount {unexpected}, which the restart does not replace"
        )));
    }
    for plan in plans {
        let source = sources.get(&plan.volume().name).ok_or_else(|| {
            conflict(format!(
                "Restart target records no log directory for mount {}",
                plan.volume().name
            ))
        })?;
        ensure_fresh_sibling(plan.volume(), source)?;
    }
    Ok(sources)
}

/// `source` 得是 `volume`原来绑定的目录的同级新目录
fn ensure_fresh_sibling(volume: &AppResourceIdentity, source: &str) -> Result<()> {
    let previous = Path::new(&volume.uid);
    let path = Path::new(source);
    if !path.is_absolute() || path.parent() != previous.parent() || source == volume.uid {
        return Err(conflict(format!(
            "Restart target records an unexpected log directory for mount {}",
            volume.name
        )));
    }
    Ok(())
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
