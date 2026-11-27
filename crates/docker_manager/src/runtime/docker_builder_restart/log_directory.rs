//! 替代 builder 的日志目录。
//!
//! 归档重放时，落在可回收日志根下面的日志 bind 不照原样重放：原容器绑定过的目录可能已经过期，
//! 日志清理器随时在回收它。替代容器的每一条这样的 bind 都换成同级的新目录（独占新建，见
//! `crate::log_directory`），并登记为正在准备挂载。
//!
//! # 哪些挂载换
//!
//! 先看**角色**——这条挂载是不是 RCoder 为这个容器独占新建的日志目录，再看路径：
//!
//! - **角色**（[`is_log_mount`]）：归档带着原容器的日志挂载记录（容器 label
//!   `rcoder.io/log-mounts`，创建容器时写的，每条是挂载点和 RCoder 新建的那个目录；一条日志
//!   挂载都没有的容器记的是空记录），挂载点和源都对得上的才算。只有升级之前建的容器才没有这份
//!   记录，那时只认创建时写进环境变量的那两个值：挂载点就是 `CONTAINER_LOGS_DIR`，**并且**源的
//!   文件名就是 `CONTAINER_LOG_NAME`——两个值一起对上同一条 bind。挂载点落在 `/home/user` 整棵
//!   里的不是（持久数据）。
//! - **路径**：源在可回收日志根下面（`host_path_space::LogDomain`，用的是清理器那个路径
//!   空间），或者判不出来。
//!
//! 两条都满足才换。角色是主判据：日志根的配置一旦盖住别的东西（自定义数据目录），只按路径挑
//! 会把一份有数据的目录换成空的新目录，数据就丢了。名字像不像日志目录、挂载点怎么拼写都**不是
//! 证据**：带数据的目录也可以叫 `<什么>-<时间戳>`。
//!
//! # 判不出来的时候
//!
//! 判不出源在不在日志根里，不让恢复失败：是日志挂载的尽量换新目录（日志目录是一次性的，
//! 不该复用；旧的那个可能早已过期，清理器正在回收）。这只是稳妥起见：新目录建不出来（比如
//! 那个位置 RCoder 看不到）就照原样重放，不让恢复失败。
//!
//! 其余的照归档原样重放，源登记到替代容器建出来。清理器对判不出来的源同样判不出来，而它
//! 确认不了就不删。
//!
//! # 不是日志挂载的
//!
//! 永不换源，原样重放并登记——归档没有记录、环境变量也对不上的 bind 也算这一类：来源不可信，
//! 就当它可能是数据，保留原 bind。源确定落在日志根里的记 error：那是配置错误（数据目录被
//! 日志根盖住了），builder 停够保留期之后清理器会回收它。拒绝恢复救不了这份数据，恢复照常。
//!
//! 换源记在替代容器的 label 上，续做和引擎都从那里读，见 `log_label`。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use container_runtime_api::ContainerRuntimeResult as Result;
use shared_types::{AppResourceIdentity, AppResourceKind, BuilderRestartTemplate};

use super::{Archive, DockerRuntime, conflict};
use crate::log_directory::{DirectoryBeingReclaimed, LogMountRecord, PendingBinds};
use crate::runtime::docker_runtime::host_path_space::{
    HostPathSpace, LogDomainAdmission, LogDomainVerdict, bind_source, create_host_directory,
};

const CONTAINER_LOGS_DIR_ENV: &str = "CONTAINER_LOGS_DIR";
const CONTAINER_LOG_NAME_ENV: &str = "CONTAINER_LOG_NAME";

impl DockerRuntime {
    /// 这次操作用的路径空间和日志域。定位不完整也不让恢复失败，见 `LogDomainAdmission`
    pub(in crate::runtime) async fn log_domain(&self) -> LogDomainAdmission<'_> {
        LogDomainAdmission::locate(
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
    volume: &'a AppResourceIdentity,
    /// 源确定落在日志根里：必须换，新目录建不出来就恢复失败。否则只是尽量换（源判不出来、
    /// 靠归档核实它是日志挂载），换不了就照原样重放
    required: bool,
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

/// 归档里的 bind 怎么处置
#[derive(Default)]
pub(super) struct LogBinds<'a> {
    /// 要换成新目录的，每条一个换源计划
    pub(super) plans: Vec<LogMountPlan<'a>>,
    /// 照归档原样重放、但源要登记到替代容器建出来的：判不出在不在日志根里的，和落在日志根
    /// 里的持久数据
    pub(super) replayed: Vec<PathBuf>,
}

/// 定下归档里每条 bind 怎么处置：哪些换成新目录，哪些原样重放但要登记。
///
/// 日志挂载的源在日志根下、却没有挂载见证（或者见证和 bind 对不上）：改不了它，原样重放又会
/// 绑到旧目录，拒绝。来源不可信的 bind 不在这个规矩里——它原样重放、登记，本来就是对的。
pub(super) async fn plan<'a>(
    template: &'a BuilderRestartTemplate,
    archive: &Archive,
    admission: &LogDomainAdmission<'_>,
) -> Result<LogBinds<'a>> {
    let mut binds = LogBinds::default();
    for volume in template
        .volumes
        .iter()
        .filter(|volume| volume.kind == AppResourceKind::HostPath)
    {
        let verdict = admission
            .classify(&format!("Mount {}", volume.name), &volume.uid)
            .await;
        if shared_types::is_builder_persistent_data_mount(&volume.name) {
            // 持久数据永不换源：重启前后必须是同一个目录。落在日志根里是配置错误：builder 停够
            // 保留期之后，清理器会回收它。记 error，恢复照常——拒绝恢复救不了这份数据
            if verdict != LogDomainVerdict::Outside {
                if verdict == LogDomainVerdict::Inside {
                    tracing::error!(
                        mount = %volume.name,
                        source = %volume.uid,
                        "A persistent data directory of the builder lies under the reclaimable log root; the log cleaner will reclaim it once the builder has been stopped for longer than the retention. Move the data directory or cleanup_config.log_cleanup.log_dir"
                    );
                }
                binds.replayed.push(PathBuf::from(&volume.uid));
            }
            continue;
        }
        // 角色是主判据：不是日志挂载的永不换源，哪怕源落在日志根里
        if !is_log_mount(&volume.name, &volume.uid, archive) {
            if verdict == LogDomainVerdict::Inside {
                tracing::error!(
                    mount = %volume.name,
                    source = %volume.uid,
                    "A mount of the builder that is not a one-time log directory lies under the reclaimable log root; it keeps its source, and the log cleaner will reclaim it once the builder has been stopped for longer than the retention. Move it out of cleanup_config.log_cleanup.log_dir"
                );
                binds.replayed.push(PathBuf::from(&volume.uid));
            }
            continue;
        }
        let required = verdict == LogDomainVerdict::Inside;
        // 是日志挂载就换，不看判定结果：日志目录是一次性的，不该复用，旧的那个可能早已过期，
        // 清理器正在回收。源确定在日志根外时清理器碰不到它，旧目录原样留着，换的代价只是容器
        // 里看不到上一次的日志
        let swap = required || admission.space().is_some();
        if !swap {
            continue;
        }
        // 按见证（源加挂载点）整段前缀去找那条 bind，不按冒号切分：源路径里有 Windows 盘符时
        // 切分有歧义。
        let archived = format!("{}:{}", volume.uid, volume.name);
        let bind = archive
            .binds
            .iter()
            .position(|bind| bind == &archived || bind.starts_with(&format!("{archived}:")));
        let previous = Path::new(&volume.uid);
        let directory = (
            previous.parent(),
            previous.file_name().and_then(|name| name.to_str()),
        );
        let (Some(bind), Some(parent), Some(previous_name)) = (bind, directory.0, directory.1)
        else {
            if required {
                return Err(conflict(format!(
                    "Restart log bind does not match the archived mount witness for {}",
                    volume.name
                )));
            }
            // 只是尽量换的那条改不了：照原样重放，下面的循环会登记它的源
            continue;
        };
        binds.plans.push(LogMountPlan {
            volume,
            required,
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
        // 有见证的卷上面已经处置过（持久数据落进 replayed），不重复登记
        if binds.replayed.iter().any(|replayed| replayed == source) {
            continue;
        }
        match admission.classify(bind, source).await {
            LogDomainVerdict::Inside => {
                // 没有挂载见证，认不出挂载点，只能按源认。记录里就有这个源目录的是日志挂载：
                // 换源要改 bind 字符串，没有挂载点就写不回去，拒绝
                if is_recorded_log_source(source, archive) {
                    return Err(conflict(format!(
                        "Restart bind {bind} lies under the reclaimable log root but has no mount witness"
                    )));
                }
                // 其余是落在日志根里的自定义挂载：不换源，照原样重放并登记
                tracing::error!(
                    bind = %bind,
                    "A bind of the builder that is not a one-time log directory lies under the reclaimable log root and has no mount witness; it keeps its source, and the log cleaner will reclaim it once the builder has been stopped for longer than the retention. Move it out of cleanup_config.log_cleanup.log_dir"
                );
                binds.replayed.push(PathBuf::from(source));
            }
            LogDomainVerdict::Outside => {}
            LogDomainVerdict::Unknown(_) => binds.replayed.push(PathBuf::from(source)),
        }
    }
    Ok(binds)
}

/// 这条挂载是不是 RCoder 为这个容器独占新建的日志目录（角色）。
///
/// 只认创建时的记录，见模块说明：
///
/// - 归档带着日志挂载记录（空记录也算带着）：挂载点和源都对得上才算。源和记录不一样，说明记录
///   之外有人动过这条 bind，不算；
/// - 归档没有记录（升级之前建的容器）：创建时写进环境变量的那两个值要**一起**对上这一条 bind
///   ——`CONTAINER_LOGS_DIR` 就是这个挂载点，并且 `CONTAINER_LOG_NAME` 就是这个源目录的名字。
///   两个值说的是同一个日志目录，谁都不能单独作证：别的父目录下的数据目录可以和日志目录同名。
///
/// 目录名的格式、挂载点的拼写都不是证据：带数据的目录也可以叫 `<什么>-<时间戳>`
pub(super) fn is_log_mount(target: &str, source: &str, archive: &Archive) -> bool {
    if shared_types::is_builder_persistent_data_mount(target) {
        return false;
    }
    match archive.log_mounts() {
        Some(record) => record
            .iter()
            .any(|mount| mount.target == target && mount.source == source),
        None => archive.log_target() == Some(target) && archive.names_log_directory(source),
    }
}

fn file_name(path: &str) -> Option<&str> {
    Path::new(path).file_name().and_then(|name| name.to_str())
}

/// 归档里没有挂载见证的 bind（归档里有、模板的卷里没有）没有挂载点可用，只能按源认：记录里
/// 就有这个源目录的，是日志挂载。旧归档没有记录，这里又没有挂载点去和 `CONTAINER_LOGS_DIR`
/// 对——单凭源的文件名不够，证据不足
fn is_recorded_log_source(source: &str, archive: &Archive) -> bool {
    archive
        .log_mounts()
        .is_some_and(|record| record.iter().any(|mount| mount.source == source))
}

/// 替代容器的日志挂载记录：归档记录里没换过源的照抄，换过源的记新目录——它们是 RCoder 这一次
/// 独占新建的。归档没有记录的（升级之前建的容器）只有换过源的这些
pub(super) fn replacement_log_mounts(
    archive: &Archive,
    fresh: &FreshLogDirectories,
) -> Vec<LogMountRecord> {
    let mut records: Vec<LogMountRecord> = archive
        .log_mounts()
        .unwrap_or_default()
        .iter()
        .filter(|record| !fresh.sources.contains_key(&record.target))
        .cloned()
        .collect();
    records.extend(fresh.sources.iter().map(|(target, source)| LogMountRecord {
        target: target.clone(),
        source: source.clone(),
    }));
    records.sort_by(|a, b| a.target.cmp(&b.target));
    records
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
    /// `replayed` 是调用方为原样重放的 bind（`binds.replayed`）已经登记好的守卫，在这里接
    /// 手：它要一直拿到替代容器建出来。`reclaim_root` 是可回收的日志根（RCoder 进程里的
    /// 路径）：新目录建在它下面更深处时，要刷新上一级候选的修改时间。
    pub(super) async fn create(
        space: Option<&HostPathSpace<'_>>,
        binds: &LogBinds<'_>,
        replayed: PendingBinds,
        reclaim_root: &Path,
    ) -> Result<Self> {
        let mut fresh = Self {
            sources: HashMap::new(),
            created: Vec::new(),
            pending: replayed,
        };
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
                // 只是尽量换的那条换不了：照原样重放，旧目录的源登记到替代容器建出来
                Err(error) if !plan.required => {
                    tracing::warn!(
                        mount = %plan.volume.name,
                        %error,
                        "Cannot create a new directory for a log mount whose source cannot be classified; its archived directory is used as it is and registered until the replacement is created"
                    );
                    fresh
                        .pending
                        .claim(Path::new(&plan.volume.uid), Some(space))
                        .await
                        .map_err(DirectoryBeingReclaimed::into_container_creation_error)?;
                    continue;
                }
                Err(error) => {
                    fresh.discard_created().await;
                    return Err(conflict(format!(
                        "Cannot create a new log directory for the replacement builder, mount {}: {error}",
                        plan.volume.name
                    )));
                }
            };
            crate::log_directory::refresh_candidate_ancestor(&created, reclaim_root).await;
            // 名字可能换过：被占用的名字不复用
            let name = created
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&plan.preferred));
            let source = plan.parent.join(name).to_string_lossy().into_owned();
            fresh
                .pending
                .claim(Path::new(&source), Some(space))
                .await
                .map_err(DirectoryBeingReclaimed::into_container_creation_error)?;
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

    /// 有没有换过源的挂载：一条都没换成时不写 label
    pub(super) fn has_sources(&self) -> bool {
        !self.sources.is_empty()
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
    pub(super) fn log_target(&self) -> Option<&str> {
        self.env
            .iter()
            .find_map(|entry| entry.strip_prefix(&format!("{CONTAINER_LOGS_DIR_ENV}=")))
    }

    /// 归档环境变量 `name` 的值
    pub(super) fn env_value(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .find_map(|entry| entry.strip_prefix(&format!("{name}=")))
    }

    /// 原容器创建时的日志挂载记录。升级之前建的容器没有
    pub(super) fn log_mounts(&self) -> Option<&[LogMountRecord]> {
        self.log_mounts.as_deref()
    }

    /// 归档的 `CONTAINER_LOG_NAME` 就是这个源目录的名字。空值不算
    fn names_log_directory(&self, source: &str) -> bool {
        self.env_value(CONTAINER_LOG_NAME_ENV)
            .is_some_and(|name| !name.is_empty() && file_name(source) == Some(name))
    }
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
