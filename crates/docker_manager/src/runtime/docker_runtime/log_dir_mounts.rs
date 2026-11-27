//! 日志目录的挂载判定：一个候选日志目录是否仍被某个容器当挂载源。
//!
//! # 两边必须在同一个路径空间里比
//!
//! 创建容器时，`apply_auto_mounts` 把容器内的日志路径按 RCoder 自身的挂载表做**词法**前缀
//! 替换，得到的宿主机路径原样交给 Docker；Docker 清单里的 `Source` 也是这个词法路径。
//! 所以第一步必须用一模一样的推导去比。只对候选解析符号链接再去比，会让一个仍在挂载的
//! 目录被判成未挂载而被删掉。
//!
//! 光有词法比对还不够：同一个目录可以有两个名字。候选走真实路径、容器经别名登记（或者
//! 反过来）时，两边的字符串对不上。所以再求出每个挂载源在宿主机上的**真实位置**，按
//! **目录身份**比第二次。身份是设备号加 inode，不是路径字符串。
//!
//! # 挂载源的真实位置
//!
//! Docker 报回的挂载源是登记时的词法路径；路径上的符号链接由内核在宿主机上、挂载那一刻
//! 解析。所以求真实位置时以宿主机路径为准，一个成分一个成分往下走，每走一步经 RCoder
//! 自己的挂载去看这一级是什么（[`locate`]）。是链接就顺着它指的地方接着走：相对目标从
//! 链接所在的目录算起，绝对目标是宿主机上的路径。只采纳能核验的视图——视图里的路径换算
//! 回宿主机必须还是这个路径，被另一条挂载遮蔽的不算。
//!
//! 求真实位置的走法（[`locate`]，在 `host_path_space`）走完之后分四种：
//!
//! - 停在一个存在的对象上：它（是文件则是它的父目录）连同各级父目录都在占用中；
//! - 路径上有一级不存在：源已经消失，没有可保护的东西，也不卡住其余目录的清理——
//!   容器还在、目录被删，正是这次要修的故障现场；
//! - 从头到尾没有读到过链接，整条路径都在 RCoder 的挂载之外：与任何候选无关；
//! - 其余——读到过链接、之后却走进 RCoder 看不见的路径（看不见的那一段里可能还有链接
//!   绕回来），被另一条挂载遮蔽，`..` 回退到 RCoder 看不到的那一级，读取出错，链接层数
//!   过多：无法确认，整轮所有候选都保留。这样的源可能指向任何候选。
//!
//! # 判定规则
//!
//! 受保护的是这样的候选：它等于某个挂载源的真实位置，或者它里面包含这个真实位置。反过来
//! 不成立——RCoder 自己挂了日志总根，不会让总根下的每个子目录都被算作占用，它们各按
//! 自己的占用情况回收。
//!
//! 1. 候选按词法换算到宿主机后，等于清单里的某个挂载源，或某个挂载源位于它之下 → 仍被挂载；
//! 2. 候选的目录身份，等于某个挂载源的真实位置（文件则取其父目录）或它某级祖先的身份 →
//!    仍被挂载；
//! 3. 候选换算不到宿主机、读不到身份，或者清单里有无法确认指向何处的挂载源 → 无法确认，
//!    保留；
//! 4. 其余 → 确认未被挂载。
//!
//! # 边界
//!
//! - 容器形态下，只有落在 RCoder 自身挂载之下的路径才看得到。从头到尾都看不见的外部挂载
//!   按无关处理，前提是部署上没有外部路径经别名指回日志根——这一点从 RCoder 内部证明不了。
//!   只要在看得见的路径上读到了链接，之后就不再按词法推断：走进看不见的地方就算无法确认。
//! - 挂载别名里的符号链接在使用期间必须保持稳定。Docker 在挂载那一刻解析符号链接；之后
//!   别名被改指别处时，运行中的容器仍持有旧目录，而这里重新解析得到的是新位置，对不上。
//! - 目录身份只在 Unix 平台可读，其它平台一律按无法确认处理。
//!
//! 路径空间怎么换算（[`HostPathSpace`]）在 `host_path_space`，判定和新建目录共用。
//!
//! # 正在准备挂载的目录
//!
//! 日志目录建好到 Docker 登记这个挂载之间（可能隔着一次很慢的镜像拉取），清单里还没有它。
//! 这段时间它由创建方按路径登记（`crate::log_directory::PendingBinds`），这里和真实挂载
//! 一视同仁：登记着的目录按"仍被挂载"处理。
//!
//! 创建方判不出在不在日志根里的 bind 源也登记在这里（它用的是同一个走法，对这条源同样判不
//! 出来，所以不让创建失败，改为登记）。这样的源定不了位，本轮所有候选都保留——确认不了就不删。
//!
//! # 删除之前的最后一次确认
//!
//! 清单和登记都只是取清单那一刻的样子，之后的创建这一轮看不到。所以清理器删每个目录之前还要
//! 回来确认一次（[`begin_reclaim`]），判定和取清单时是同一个（[`classify`]）。要补的是两种交错：
//!
//! - **创建方在取完登记之后才登记**，要复用一个已过期的旧目录。确认时先给目录打上"正在删"的
//!   标记，再读登记，两步在同一次持锁里：标记之后才登记的创建方一定看得到标记、会等这次删除
//!   结束（`reclaim_wait`），所以读到的登记里没有就是真没有。反过来先读登记再打标记，两步之间
//!   登记的创建方两头都看不到。
//! - **登记已经交给了容器**：创建方登记 → Docker 把容器建出来 → 创建方注销，三步都在取完清单
//!   之后。登记表里没有它，那份 Docker 清单里也没有。所以每份视图都记着取它时登记表的代次，
//!   确认时对一下：变了就说明清单之后有创建开始或结束，在标记之下重新取一次，**整轮换成这份
//!   新的**——只给当前这一个候选用是不够的，绑着后面候选的容器同样不在旧清单里。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use bollard::models::ContainerSummary;
use container_runtime_api::{
    ContainerRuntimeError, ContainerRuntimeResult, LogDirectoryMounts, ReclaimPermit,
    ReclaimRefused,
};
use std::sync::{Arc, PoisonError};

use super::DockerRuntime;
use super::host_path_space::{
    DirectoryId, FileSystemView, HostPathSpace, LocalFileSystem, Location, RealLocations,
    is_absent, path_space, viewing,
};
use crate::{DockerError, DockerResult};

mod reclaim_wait;
pub(crate) use reclaim_wait::wait_while_reclaiming;

/// 清理器的一份视图：取清单那一刻登记表的代次，和按那份清单算好的观察结果。
///
/// 代次没变，这份视图就还代表现在：登记表里没有新的登记加上、也没有登记注销，所以取清单之后
/// 不会有新的容器建出来——创建方登记在先，登记一动代次就变
#[derive(Debug)]
pub(super) struct LogDirectoryView {
    generation: u64,
    observations: Arc<MountObservations>,
}

impl DockerRuntime {
    fn with_log_directory_view<T>(&self, read: impl FnOnce(Option<&LogDirectoryView>) -> T) -> T {
        let view = self
            .log_directory_view
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        read(view.as_ref())
    }

    fn remember_log_directory_view(&self, generation: u64, observations: Arc<MountObservations>) {
        *self
            .log_directory_view
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(LogDirectoryView {
            generation,
            observations,
        });
    }
}

/// 从一次容器列表的响应里收齐所有挂载源。
///
/// 任何让"挂载源是否完整"存疑的响应都报错，不会被当成"没有挂载"：缺挂载列表、缺挂载
/// 类型、bind 缺绝对源路径。错误信息带上容器 ID。不只收 bind：其它带宿主机路径的挂载
/// 也一并保护，将来新增的挂载类型不会漏。
pub(crate) fn mount_sources(containers: &[ContainerSummary]) -> DockerResult<HashSet<PathBuf>> {
    let mut sources = HashSet::new();
    for container in containers {
        let id = container.id.as_deref().unwrap_or("<unknown container>");
        let mounts = container.mounts.as_ref().ok_or_else(|| {
            DockerError::ConnectionError(format!(
                "Container {id} reported no mount list, so its mounts cannot be protected"
            ))
        })?;
        for mount in mounts {
            let kind = mount
                .typ
                .as_deref()
                .filter(|kind| !kind.is_empty())
                .ok_or_else(|| {
                    DockerError::ConnectionError(format!(
                        "Container {id} reported a mount without a type"
                    ))
                })?;
            let source = mount
                .source
                .as_deref()
                .filter(|source| Path::new(source).is_absolute());
            let Some(source) = source else {
                // tmpfs、镜像这类挂载没有可保护的宿主机目录
                if kind == "bind" {
                    return Err(DockerError::ConnectionError(format!(
                        "Container {id} reported a bind mount without an absolute source path"
                    )));
                }
                continue;
            };
            sources.insert(crate::normalize_path(Path::new(source)));
        }
    }
    Ok(sources)
}

/// 一组路径连同它们的全部祖先：子孙路径被挂载时，祖先目录同样不能删
fn with_ancestors(paths: impl Iterator<Item = PathBuf>) -> HashSet<PathBuf> {
    let mut expanded = HashSet::new();
    for path in paths {
        expanded.extend(path.ancestors().map(Path::to_path_buf));
    }
    expanded
}

pub(super) fn list_error(error: DockerError) -> ContainerRuntimeError {
    match error {
        DockerError::Timeout(message) => ContainerRuntimeError::Timeout(message),
        other => {
            ContainerRuntimeError::DockerError(format!("List container mount sources: {other}"))
        }
    }
}

/// 候选换算不到宿主机时的原因
fn mapping_failure(error: &DockerError) -> String {
    format!("Map a log directory onto the host: {error}")
}

/// `start` 连同它的各级父目录都在占用中，直到离开 RCoder 能核验的范围
async fn occupy_ancestors(
    space: &HostPathSpace<'_>,
    fs: &dyn FileSystemView,
    identities: &mut HashMap<PathBuf, DirectoryId>,
    start: &Path,
    mounted: &mut HashSet<DirectoryId>,
) -> Result<(), String> {
    for ancestor in start.ancestors() {
        let Some(visible) = viewing(space, ancestor).visible_path() else {
            return Ok(());
        };
        let identity = match identities.get(ancestor) {
            Some(identity) => *identity,
            None => match fs.identity(&visible).await {
                Ok(identity) => {
                    identities.insert(ancestor.to_path_buf(), identity);
                    identity
                }
                Err(error) => {
                    return Err(format!(
                        "the ancestor {} of a mount source cannot be identified from inside RCoder: {error}",
                        ancestor.display()
                    ));
                }
            },
        };
        mounted.insert(identity);
    }
    Ok(())
}

/// 判定一轮候选所需的观察结果：取一次清单、算一次，逐候选使用
#[derive(Debug)]
struct MountObservations {
    /// 词法：挂载源及其全部祖先（宿主机路径）。候选等于其中之一，就是候选包含被挂载的子路径
    registered: HashSet<PathBuf>,
    /// 身份：挂载源的真实位置（文件则取其父目录），以及它直到离开 RCoder 视野的各级祖先
    mounted: HashSet<DirectoryId>,
    /// 无法确认指向何处的挂载源。它可能指向任何候选，所以出现时所有候选都保留
    unidentifiable: Option<String>,
}

async fn observe(
    space: &HostPathSpace<'_>,
    mounted_sources: &[PathBuf],
    pending_sources: &[PathBuf],
    fs: &dyn FileSystemView,
) -> MountObservations {
    let mut observations = MountObservations {
        registered: with_ancestors(mounted_sources.iter().chain(pending_sources).cloned()),
        mounted: HashSet::new(),
        unidentifiable: None,
    };
    let mut locations = RealLocations::new(space, fs);
    let mut identities = HashMap::new();
    let sources = pending_sources
        .iter()
        .map(|source| (source, true))
        .chain(mounted_sources.iter().map(|source| (source, false)));
    for (source, pending) in sources {
        let start = match locations.locate(source).await {
            Err(error) => Err(format!(
                "mount source {} cannot be identified from inside RCoder: {error}",
                source.display()
            )),
            Ok(Location::Unconfirmed(reason)) => Err(reason),
            Ok(Location::Absent { path }) if pending => {
                pending_directory_ancestor(&mut locations, &path).await
            }
            Ok(Location::Absent { .. } | Location::Outside) => Ok(None),
            Ok(Location::Present { path, directory }) => Ok(Some(if directory {
                path
            } else {
                path.parent().map(Path::to_path_buf).unwrap_or(path)
            })),
        };
        let occupied = match start {
            Ok(Some(start)) => {
                occupy_ancestors(
                    space,
                    fs,
                    &mut identities,
                    &start,
                    &mut observations.mounted,
                )
                .await
            }
            Ok(None) => Ok(()),
            Err(reason) => Err(reason),
        };
        if let Err(reason) = occupied {
            observations.unidentifiable = Some(reason);
            break;
        }
    }
    observations
}

/// A pending bind can create a missing source before Docker sees it. Protect
/// its existing directory ancestors by identity, even across different views.
async fn pending_directory_ancestor(
    locations: &mut RealLocations<'_>,
    source: &Path,
) -> Result<Option<PathBuf>, String> {
    for ancestor in source.ancestors().skip(1) {
        match locations.locate(ancestor).await {
            Ok(Location::Present {
                path,
                directory: true,
            }) => return Ok(Some(path)),
            Ok(Location::Absent { .. }) => continue,
            // A file cannot acquire children. An ancestor outside every
            // view has no directory identity we can compare.
            Ok(
                Location::Present {
                    directory: false, ..
                }
                | Location::Outside,
            ) => return Ok(None),
            Ok(Location::Unconfirmed(reason)) => return Err(reason),
            Err(error) => {
                return Err(format!(
                    "the ancestor {} of a pending mount cannot be identified: {error}",
                    ancestor.display()
                ));
            }
        }
    }
    Ok(None)
}

/// 一个候选日志目录的判定，规则见模块说明
#[derive(Debug, Eq, PartialEq)]
enum LogDirMount {
    /// 清单里有容器挂着它
    Mounted,
    /// 确认没有容器挂它，可以回收
    Unmounted,
    /// 无法确认，调用方必须保留
    Unconfirmed(String),
}

async fn classify(
    candidate: &Path,
    space: &HostPathSpace<'_>,
    observations: &MountObservations,
    fs: &dyn FileSystemView,
) -> LogDirMount {
    // 1. 与创建侧完全相同的推导
    let registered_path = match space.to_host(candidate) {
        Ok(path) => crate::normalize_path(&path),
        Err(error) => return LogDirMount::Unconfirmed(mapping_failure(&error)),
    };
    if observations.registered.contains(&registered_path) {
        return LogDirMount::Mounted;
    }

    // 2. 目录身份：同一个目录可以经别名、经另一个挂载点出现，路径对不上，身份对得上
    let identity = match fs.identity(candidate).await {
        Ok(identity) => identity,
        // 列目录之后目录已经消失，没有留下可被挂载的实体
        Err(error) if is_absent(&error) => {
            return LogDirMount::Unmounted;
        }
        Err(error) => {
            return LogDirMount::Unconfirmed(format!(
                "its directory identity cannot be read: {error}"
            ));
        }
    };
    if observations.mounted.contains(&identity) {
        return LogDirMount::Mounted;
    }

    // 3. 有对不上号的挂载源：它可能是这个候选的别名
    if let Some(reason) = &observations.unidentifiable {
        return LogDirMount::Unconfirmed(reason.clone());
    }
    LogDirMount::Unmounted
}

/// 判定每个候选日志目录是否仍被容器挂载。这一轮的视图同时留给 [`begin_reclaim`]。
///
/// 清单一律取自真实 Docker，不读进程内缓存，一轮取一次；另加上登记表里正在准备挂载的目录
/// （在途的和结果不明的）。单个候选换算失败或读不到身份只影响它自己，记为无法确认；只有整份
/// 清单拿不到时才返回 `Err`
pub(super) async fn mounted_log_directories(
    runtime: &DockerRuntime,
    candidates: &[PathBuf],
) -> ContainerRuntimeResult<LogDirectoryMounts> {
    if candidates.is_empty() {
        return Ok(LogDirectoryMounts::default());
    }
    // 先读登记表（连同代次），再取 Docker 清单，顺序不能反。登记在 Docker 应答之后才注销：
    // 一个目录要么此刻登记着，要么注销时容器已经登记进 Docker、会出现在之后取的清单里。反过来
    // 先取清单再读登记会两头落空
    let registry = crate::log_directory::registry_view();
    let mounted_sources: Vec<PathBuf> = runtime
        .inner
        .mounted_host_paths()
        .await
        .map_err(list_error)?
        .into_iter()
        .collect();

    let space = path_space(&runtime.inner, &runtime.log_path_resolver).await?;
    let fs = LocalFileSystem;
    let observations = Arc::new(observe(&space, &mounted_sources, &registry.sources, &fs).await);
    runtime.remember_log_directory_view(registry.generation, Arc::clone(&observations));

    let mut mounts = LogDirectoryMounts::default();
    for candidate in candidates {
        match classify(candidate, &space, &observations, &fs).await {
            LogDirMount::Mounted => {
                mounts.mounted.insert(candidate.clone());
            }
            LogDirMount::Unmounted => {}
            LogDirMount::Unconfirmed(reason) => {
                mounts.unconfirmed.insert(candidate.clone(), reason);
            }
        }
    }
    Ok(mounts)
}

/// 清理器删一个候选目录之前的最后一次确认，见模块说明"删除之前的最后一次确认"。
///
/// 许可（[`ReclaimPermit`]）由调用方拿到删除结束：它 drop 时才解除"正在删"的标记，登记着
/// 这个目录的创建方也在那之前一直等。
///
/// 用的是这一轮的视图（登记、Docker 清单，连同取它时的代次）。代次变了——清单之后有创建开始
/// 或者结束——旧视图已经不代表现在：标记打上之后重新取一次，整轮换成新的。只给当前这个候选用
/// 是不够的：绑着后面某个候选的容器同样不在旧清单里
pub(super) async fn begin_reclaim(
    runtime: &DockerRuntime,
    candidate: &Path,
) -> Result<ReclaimPermit, ReclaimRefused> {
    let space = path_space(&runtime.inner, &runtime.log_path_resolver)
        .await
        .map_err(|error| ReclaimRefused::Unconfirmed(error.to_string()))?;
    let registered = match space.to_host(candidate) {
        Ok(path) => crate::normalize_path(&path),
        Err(error) => return Err(ReclaimRefused::Unconfirmed(mapping_failure(&error))),
    };
    // 一次持锁：打标记，读登记，读代次。之后的登记一定看得到标记
    let (mark, registry) = crate::log_directory::mark_reclaim(&registered, candidate)
        .map_err(ReclaimRefused::Unconfirmed)?;
    let still_current = runtime.with_log_directory_view(|view| {
        view.filter(|view| view.generation == registry.generation)
            .map(|view| Arc::clone(&view.observations))
    });
    let fs = LocalFileSystem;
    let observations = match still_current {
        Some(observations) => observations,
        None => {
            let mounted_sources: Vec<PathBuf> = runtime
                .inner
                .mounted_host_paths()
                .await
                .map_err(|error| ReclaimRefused::Unconfirmed(list_error(error).to_string()))?
                .into_iter()
                .collect();
            let mut pending_sources = registry.sources;
            pending_sources.extend(crate::log_directory::unresolved_sources());
            let observations =
                Arc::new(observe(&space, &mounted_sources, &pending_sources, &fs).await);
            runtime.remember_log_directory_view(registry.generation, Arc::clone(&observations));
            observations
        }
    };
    match classify(candidate, &space, &observations, &fs).await {
        LogDirMount::Unmounted => Ok(ReclaimPermit::release_on_drop(move || drop(mark))),
        LogDirMount::Mounted => Err(ReclaimRefused::InUse(format!(
            "a starting container has registered {} or a path below it",
            registered.display()
        ))),
        LogDirMount::Unconfirmed(reason) => Err(ReclaimRefused::Unconfirmed(reason)),
    }
}

#[cfg(test)]
mod tests;
