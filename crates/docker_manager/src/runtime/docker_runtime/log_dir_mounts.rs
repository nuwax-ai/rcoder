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
//! 走完之后分四种：
//!
//! - 停在一个存在的对象上：它（是文件则是它的父目录）连同各级父目录都在占用中；
//! - 路径上有一级不存在：源已经消失，没有可保护的东西，也不卡住其余目录的清理——
//!   容器还在、目录被删，正是这次要修的故障现场；
//! - 整条路径都在 RCoder 的挂载之外：与任何候选无关；
//! - 其余——被另一条挂载遮蔽、`..` 回退到 RCoder 看不到的那一级、读取出错、链接层数
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
//! - 容器形态下，只有落在 RCoder 自身挂载之下的路径才看得到。第三方容器经由 RCoder 看不到
//!   的宿主机别名挂载同一个目录，从容器内识别不了；RCoder 看不到的那段路径里如果有符号
//!   链接，词法上的推断也可能不对。
//! - 挂载别名里的符号链接在使用期间必须保持稳定。Docker 在挂载那一刻解析符号链接；之后
//!   别名被改指别处时，运行中的容器仍持有旧目录，而这里重新解析得到的是新位置，对不上。
//! - 目录身份只在 Unix 平台可读，其它平台一律按无法确认处理。

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use bollard::models::ContainerSummary;
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult, LogDirectoryMounts};
use tokio::sync::OnceCell;

use crate::{DockerError, DockerManager, DockerResult, HostPathResolver};

/// 一条路径上最多跟随多少个符号链接，与内核的上限相同
const MAX_SYMLINKS: usize = 40;

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
fn with_ancestors(paths: impl IntoIterator<Item = PathBuf>) -> HashSet<PathBuf> {
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

/// RCoder 进程命名空间里的路径如何与宿主机路径互相换算
enum HostPathSpace<'a> {
    /// deploy-host 形态：RCoder 与 daemon 同在宿主机，进程路径就是宿主机路径
    Host,
    /// 容器形态：按 RCoder 所在容器的挂载表替换前缀
    Container(&'a HostPathResolver),
}

/// 一个宿主机路径在 RCoder 命名空间里的一个视图：这个宿主机路径经某条挂载看到的路径
struct Projection {
    path: PathBuf,
}

impl HostPathSpace<'_> {
    /// 与创建侧 `apply_auto_mounts` 登记挂载时逐字一致：先做词法归一化，再替换前缀
    fn to_host(&self, path: &Path) -> DockerResult<PathBuf> {
        let lexical = crate::normalize_path(path);
        match self {
            HostPathSpace::Host => Ok(lexical),
            HostPathSpace::Container(resolver) => resolver.resolve_to_host_path(&lexical),
        }
    }

    /// `host_path` 在 RCoder 命名空间里的全部视图：同一个宿主机目录挂了几次就有几个
    fn projections(&self, host_path: &Path) -> Vec<Projection> {
        let host_path = crate::normalize_path(host_path);
        match self {
            HostPathSpace::Host => vec![Projection { path: host_path }],
            HostPathSpace::Container(resolver) => resolver
                .projections_from_host_path(&host_path)
                .into_iter()
                .map(|(_, path)| Projection { path })
                .collect(),
        }
    }

    /// 这个视图看到的确实是 `host_path`：把视图里的路径再正向换算回去，必须得到 `host_path`。
    /// 路径中途被另一条挂载遮蔽时，看到的是另一棵目录，换算回去就对不上。
    fn shows(&self, projection: &Projection, host_path: &Path) -> bool {
        self.to_host(&projection.path)
            .is_ok_and(|mapped| mapped == crate::normalize_path(host_path))
    }
}

/// 候选换算不到宿主机时的原因
fn mapping_failure(error: &DockerError) -> String {
    format!("Map a log directory onto the host: {error}")
}

/// 目录的身份：设备号加 inode。同一个目录经不同名字、不同挂载点出现时身份相同
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
struct DirectoryId(u64, u64);

/// 一个路径本身是什么，不跟随符号链接
#[derive(Clone, Debug, Eq, PartialEq)]
enum Entry {
    Directory,
    /// 普通文件，或其它既不是目录也不是链接的对象
    Other,
    Link(PathBuf),
    /// 不存在；路径中间有一级不是目录时也是它
    Missing,
}

/// 读取文件系统的来源
///
/// 生产环境读真实文件系统。做成可替换，是为了在没有特权做 bind mount 的测试里模拟"同一个
/// 宿主机目录挂进 RCoder 两次"。
#[async_trait]
trait FileSystemView: Send + Sync {
    /// 这个路径本身是什么，不跟随符号链接
    async fn entry(&self, path: &Path) -> std::io::Result<Entry>;
    /// 跟随符号链接后这个路径所指目录的身份；不是目录等错误原样返回
    async fn identity(&self, path: &Path) -> std::io::Result<DirectoryId>;
}

struct LocalFileSystem;

#[async_trait]
impl FileSystemView for LocalFileSystem {
    async fn entry(&self, path: &Path) -> std::io::Result<Entry> {
        let metadata = match tokio::fs::symlink_metadata(path).await {
            Err(error) if is_absent(&error) => return Ok(Entry::Missing),
            inspected => inspected?,
        };
        if metadata.file_type().is_symlink() {
            return tokio::fs::read_link(path).await.map(Entry::Link);
        }
        Ok(if metadata.is_dir() {
            Entry::Directory
        } else {
            Entry::Other
        })
    }

    async fn identity(&self, path: &Path) -> std::io::Result<DirectoryId> {
        let metadata = tokio::fs::metadata(path).await?;
        if !metadata.is_dir() {
            return Err(std::io::Error::from(ErrorKind::NotADirectory));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(DirectoryId(metadata.dev(), metadata.ino()))
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Err(std::io::Error::new(
                ErrorKind::Unsupported,
                "directory identity is only available on Unix",
            ))
        }
    }
}

/// 路径不存在，或者路径中间有一级不是目录
fn is_absent(error: &std::io::Error) -> bool {
    matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
}

/// 一个宿主机路径经哪个视图看得到
enum Viewed {
    Through(Projection),
    /// RCoder 的任何挂载都不覆盖它
    Unmounted,
    /// 有挂载覆盖，但每个视图都对不上：这个位置被另一条挂载遮蔽了
    Hidden,
}

/// 取最具体的那个能核验的视图。视图里的路径要能换算回这个宿主机路径才算数：路径中途被
/// 另一条挂载遮蔽时，看到的是另一棵目录，换算回去就对不上
fn viewing(space: &HostPathSpace<'_>, host_path: &Path) -> Viewed {
    let mut hidden = false;
    for projection in space.projections(host_path) {
        if space.shows(&projection, host_path) {
            return Viewed::Through(projection);
        }
        hidden = true;
    }
    if hidden {
        Viewed::Hidden
    } else {
        Viewed::Unmounted
    }
}

/// 经 RCoder 的挂载看到的一个宿主机路径
#[derive(Clone, Debug, Eq, PartialEq)]
enum Sight {
    Seen(Entry),
    Unmounted,
    Hidden,
}

/// 看一个宿主机路径是什么；同一轮里同一个路径只看一次
async fn look(
    space: &HostPathSpace<'_>,
    fs: &dyn FileSystemView,
    sights: &mut HashMap<PathBuf, Sight>,
    host_path: &Path,
) -> std::io::Result<Sight> {
    if let Some(sight) = sights.get(host_path) {
        return Ok(sight.clone());
    }
    let sight = match viewing(space, host_path) {
        Viewed::Through(projection) => Sight::Seen(fs.entry(&projection.path).await?),
        Viewed::Unmounted => Sight::Unmounted,
        Viewed::Hidden => Sight::Hidden,
    };
    sights.insert(host_path.to_path_buf(), sight.clone());
    Ok(sight)
}

/// 一个挂载源在宿主机上的位置
#[derive(Debug, Eq, PartialEq)]
enum Location {
    /// 解析到了一个存在的对象
    Present { path: PathBuf, directory: bool },
    /// 路径上有一级不存在
    Absent,
    /// 整条路径都在 RCoder 的挂载之外
    Outside,
    /// 从 RCoder 这里无法确认它指向哪里
    Unconfirmed(String),
}

/// 一条路径里要逐级走过去的成分：去掉根和盘符，保留 `.` 和 `..`
fn components(path: &Path) -> impl Iterator<Item = OsString> {
    path.components()
        .filter(|component| !matches!(component, Component::RootDir | Component::Prefix(_)))
        .map(|component| component.as_os_str().to_os_string())
}

/// 从根开始把走过的成分接成绝对路径
fn from_root(resolved: &[OsString]) -> PathBuf {
    let mut path = PathBuf::from("/");
    for component in resolved {
        path.push(component);
    }
    path
}

/// 以宿主机路径为准，一个成分一个成分往下走，求挂载源的真实位置；做法见模块说明
async fn locate(
    space: &HostPathSpace<'_>,
    fs: &dyn FileSystemView,
    sights: &mut HashMap<PathBuf, Sight>,
    source: &Path,
) -> std::io::Result<Location> {
    let unconfirmed = |detail: String| -> std::io::Result<Location> {
        Ok(Location::Unconfirmed(format!(
            "mount source {}: {detail}",
            source.display()
        )))
    };
    let mut resolved: Vec<OsString> = Vec::new();
    // `resolved` 里每一级是否经视图核验过
    let mut confirmed: Vec<bool> = Vec::new();
    let mut last = Entry::Missing;
    let mut queue: VecDeque<OsString> = components(source).collect();
    let mut links = 0usize;
    while let Some(component) = queue.pop_front() {
        if component == ".." {
            match confirmed.pop() {
                Some(true) => {
                    resolved.pop();
                }
                Some(false) => {
                    return unconfirmed(
                        "its `..` climbs above the mounts RCoder can verify, so where it leads is unknown"
                            .to_owned(),
                    );
                }
                None => {}
            }
            continue;
        }
        if component == "." {
            continue;
        }
        resolved.push(component);
        let path = from_root(&resolved);
        match look(space, fs, sights, &path).await? {
            Sight::Seen(entry @ (Entry::Directory | Entry::Other)) => {
                confirmed.push(true);
                last = entry;
            }
            Sight::Seen(Entry::Link(target)) => {
                links += 1;
                if links > MAX_SYMLINKS {
                    return unconfirmed(format!(
                        "it passes through more than {MAX_SYMLINKS} symbolic links"
                    ));
                }
                // 链接自己被它的目标取代：相对目标从链接所在的目录接着走
                resolved.pop();
                if target.is_absolute() {
                    resolved.clear();
                    confirmed.clear();
                }
                queue = components(&target).chain(queue).collect();
            }
            Sight::Seen(Entry::Missing) => return Ok(Location::Absent),
            Sight::Unmounted => confirmed.push(false),
            Sight::Hidden => {
                return unconfirmed(
                    "it is hidden behind another mount, so it cannot be identified from inside RCoder"
                        .to_owned(),
                );
            }
        }
    }
    Ok(if confirmed.last() == Some(&true) {
        Location::Present {
            path: from_root(&resolved),
            directory: last == Entry::Directory,
        }
    } else {
        Location::Outside
    })
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
        let Viewed::Through(projection) = viewing(space, ancestor) else {
            return Ok(());
        };
        let identity = match identities.get(ancestor) {
            Some(identity) => *identity,
            None => match fs.identity(&projection.path).await {
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
    let mut sights = HashMap::new();
    let mut identities = HashMap::new();
    for source in sources {
        let unconfirmed = match locate(space, fs, &mut sights, source).await {
            Err(error) => Some(format!(
                "mount source {} cannot be identified from inside RCoder: {error}",
                source.display()
            )),
            Ok(Location::Unconfirmed(reason)) => Some(reason),
            Ok(Location::Absent | Location::Outside) => None,
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

/// 解析器只在首次用到时创建，并且必须用取清单的同一个 Docker 客户端：两者观察的得是
/// 同一个 daemon、同一个"自己"，否则换算出的宿主机路径和清单对不上。
async fn container_resolver<'a>(
    manager: &DockerManager,
    cell: &'a OnceCell<HostPathResolver>,
) -> ContainerRuntimeResult<&'a HostPathResolver> {
    let client = manager.get_docker_client().clone();
    tokio::time::timeout(
        Duration::from_secs(manager.config.api_timeout_quick_seconds),
        cell.get_or_try_init(|| HostPathResolver::new_with_docker_client(client)),
    )
    .await
    .map_err(|_| {
        ContainerRuntimeError::Timeout("Resolve log directory mounts timed out".to_owned())
    })?
    .map_err(|error| {
        ContainerRuntimeError::DockerError(format!("Resolve log directory mounts: {error}"))
    })
}

/// 判定每个候选日志目录是否仍被容器挂载。
///
/// 清单一律取自真实 Docker，不读进程内缓存，整轮只发一次列表请求。单个候选换算失败或读不到
/// 身份只影响它自己，记为无法确认；只有整份清单拿不到时才返回 `Err`。
pub(super) async fn mounted_log_directories(
    manager: &DockerManager,
    log_path_resolver: &OnceCell<HostPathResolver>,
    candidates: &[PathBuf],
) -> ContainerRuntimeResult<LogDirectoryMounts> {
    if candidates.is_empty() {
        return Ok(LogDirectoryMounts::default());
    }
    let sources: Vec<PathBuf> = manager
        .mounted_host_paths()
        .await
        .map_err(list_error)?
        .into_iter()
        .collect();

    let space = match log_path_resolver.get() {
        // 显式注入的挂载表优先：调用方已经知道映射关系，不再自检
        Some(resolver) => HostPathSpace::Container(resolver),
        None if shared_types::is_deploy_host() => HostPathSpace::Host,
        None => HostPathSpace::Container(container_resolver(manager, log_path_resolver).await?),
    };
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
