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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use bollard::models::ContainerSummary;
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult, LogDirectoryMounts};
use tokio::sync::OnceCell;

use super::host_path_space::{
    DirectoryId, FileSystemView, HostPathSpace, LocalFileSystem, Location, RealLocations,
    is_absent, path_space, viewing,
};
use crate::{DockerError, DockerManager, DockerResult, HostPathResolver};

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
    sources: &[PathBuf],
    fs: &dyn FileSystemView,
) -> MountObservations {
    let mut observations = MountObservations {
        registered: with_ancestors(sources.iter().cloned()),
        mounted: HashSet::new(),
        unidentifiable: None,
    };
    let mut locations = RealLocations::new(space, fs);
    let mut identities = HashMap::new();
    for source in sources {
        let unconfirmed = match locations.locate(source).await {
            Err(error) => Some(format!(
                "mount source {} cannot be identified from inside RCoder: {error}",
                source.display()
            )),
            Ok(Location::Unconfirmed(reason)) => Some(reason),
            Ok(Location::Absent { .. } | Location::Outside) => None,
            Ok(Location::Present { path, directory }) => {
                let start = if directory {
                    path
                } else {
                    path.parent().map(Path::to_path_buf).unwrap_or(path)
                };
                occupy_ancestors(
                    space,
                    fs,
                    &mut identities,
                    &start,
                    &mut observations.mounted,
                )
                .await
                .err()
            }
        };
        if let Some(reason) = unconfirmed {
            observations.unidentifiable = Some(reason);
            break;
        }
    }
    observations
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

/// 判定每个候选日志目录是否仍被容器挂载。
///
/// 清单一律取自真实 Docker，不读进程内缓存，整轮只发一次列表请求；另加上正在准备挂载的
/// 目录的占用登记（`crate::log_directory::pending_bind_sources`）。单个候选换算失败或读不到
/// 身份只影响它自己，记为无法确认；只有整份清单拿不到时才返回 `Err`。
pub(super) async fn mounted_log_directories(
    manager: &DockerManager,
    log_path_resolver: &OnceCell<HostPathResolver>,
    candidates: &[PathBuf],
) -> ContainerRuntimeResult<LogDirectoryMounts> {
    if candidates.is_empty() {
        return Ok(LogDirectoryMounts::default());
    }
    // 先读占用登记，再取 Docker 清单，顺序不能反。登记在 Docker 应答之后才注销：一个目录
    // 要么此刻登记着，要么注销时容器已经登记进 Docker、会出现在之后取的清单里。反过来先取
    // 清单再读登记会两头落空。
    let pending = crate::log_directory::pending_bind_sources();
    let sources: Vec<PathBuf> = pending
        .into_iter()
        .chain(manager.mounted_host_paths().await.map_err(list_error)?)
        .collect();

    let space = path_space(manager, log_path_resolver).await?;
    let fs = LocalFileSystem;
    let observations = observe(&space, &sources, &fs).await;

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

#[cfg(test)]
mod tests;
