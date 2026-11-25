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
//! 反过来）时，两边的字符串对不上。所以再把清单里的挂载源换算回 RCoder 的命名空间，
//! 按**目录身份**比第二次。身份是设备号加 inode，不是路径字符串：同一个宿主机目录挂进
//! RCoder 两次时，两个视图的路径不同，身份相同。
//!
//! # 判定规则
//!
//! 受保护的是这样的候选：它等于某个挂载源，或者它里面包含某个被挂载的子路径。反过来
//! 不成立——RCoder 自己挂了日志总根，不会让总根下的每个子目录都被算作占用，它们各按
//! 自己的占用情况回收。
//!
//! 1. 候选按词法换算到宿主机后，等于清单里的某个挂载源，或某个挂载源位于它之下 → 仍被挂载；
//! 2. 候选的目录身份，等于某个挂载源或它某级祖先的身份 → 仍被挂载；
//! 3. 候选换算不到宿主机、读不到身份，或者清单里有落在 RCoder 挂载之下却无法辨识的挂载源
//!    → 无法确认，保留；
//! 4. 其余 → 确认未被挂载。
//!
//! 第 3 条的后半针对"可能相关却对不上"的挂载源：它可能是任何候选的别名，所以出现时整轮
//! 所有候选都保留。挂载源本身已经不存在的不算——容器还在、目录被删，正是这次要修的故障
//! 现场，不能让它反过来卡住清理。
//!
//! # 边界
//!
//! - 容器形态下，只有落在 RCoder 自身挂载之下的挂载源才能换算回来。第三方容器经由 RCoder
//!   看不到的宿主机别名挂载同一个目录，从容器内识别不了。
//! - 挂载别名里的符号链接在使用期间必须保持稳定。Docker 在挂载那一刻解析符号链接；之后
//!   别名被改指别处时，运行中的容器仍持有旧目录，而这里重新解析得到的是新位置，对不上。
//! - 目录身份只在 Unix 平台可读，其它平台一律按无法确认处理。

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use bollard::models::ContainerSummary;
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult, LogDirectoryMounts};
use tokio::sync::OnceCell;

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

/// 一个宿主机路径在 RCoder 命名空间里的一个视图
struct Projection {
    /// 这条挂载在进程里的根
    root: PathBuf,
    /// 这个宿主机路径经这条挂载看到的路径
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
            HostPathSpace::Host => vec![Projection {
                root: host_path.clone(),
                path: host_path,
            }],
            HostPathSpace::Container(resolver) => resolver
                .projections_from_host_path(&host_path)
                .into_iter()
                .map(|(root, path)| Projection { root, path })
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

/// 目录的身份：设备号加 inode。同一个目录经不同挂载点看到时身份相同，路径字符串则可能不同
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
struct DirectoryId(u64, u64);

/// 读取目录身份的来源
///
/// 生产环境读文件系统。做成可替换，是为了在没有特权做 bind mount 的测试里模拟"同一个
/// 宿主机目录挂进 RCoder 两次"。
#[async_trait]
trait DirectoryIdentities: Send + Sync {
    /// 跟随符号链接后这个路径所指目录的身份；路径不存在、不是目录等错误原样返回
    async fn identity(&self, path: &Path) -> std::io::Result<DirectoryId>;
}

struct FileSystemIdentities;

#[async_trait]
impl DirectoryIdentities for FileSystemIdentities {
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

/// 从视图里的路径到视图根，每一级目录的身份，含两端
///
/// 被挂载的路径本身是普通文件时（文件级 bind）没有目录身份可取，但它的父目录仍要保护。
async fn identities_up_to_root(
    identities: &dyn DirectoryIdentities,
    projection: &Projection,
) -> std::io::Result<Vec<DirectoryId>> {
    let mut found = Vec::new();
    for (depth, ancestor) in projection.path.ancestors().enumerate() {
        match identities.identity(ancestor).await {
            Ok(identity) => found.push(identity),
            Err(error) if depth == 0 && error.kind() == ErrorKind::NotADirectory => {}
            Err(error) => return Err(error),
        }
        if ancestor == projection.root {
            return Ok(found);
        }
    }
    Err(std::io::Error::new(
        ErrorKind::InvalidData,
        "the root of a view is not an ancestor of the path it shows",
    ))
}

/// 判定一轮候选所需的观察结果：取一次清单、算一次，逐候选使用
struct MountObservations {
    /// 词法：挂载源及其全部祖先（宿主机路径）。候选等于其中之一，就是候选包含被挂载的子路径
    registered: HashSet<PathBuf>,
    /// 身份：被挂载的目录，以及它在所在视图里直到挂载根的各级祖先
    mounted: HashSet<DirectoryId>,
    /// 落在 RCoder 的挂载之下、却无法在进程里辨识的挂载源。它可能是任何候选的别名，
    /// 所以出现时所有候选都保留
    unidentifiable: Option<String>,
}

async fn observe(
    space: &HostPathSpace<'_>,
    sources: &[PathBuf],
    identities: &dyn DirectoryIdentities,
) -> MountObservations {
    let mut observations = MountObservations {
        registered: with_ancestors(sources.iter().cloned()),
        mounted: HashSet::new(),
        unidentifiable: None,
    };
    for source in sources {
        let projections = space.projections(source);
        // 不在 RCoder 任何挂载之下的宿主机路径：这里看不到，只参与词法比对
        if projections.is_empty() {
            continue;
        }
        // 身份与视图无关：任意一个能看到这个挂载源的视图读到即可
        let Some(projection) = projections
            .iter()
            .find(|projection| space.shows(projection, source))
        else {
            observations.unidentifiable = Some(format!(
                "mount source {} is hidden behind another mount, so it cannot be identified from inside RCoder",
                source.display()
            ));
            break;
        };
        match identities_up_to_root(identities, projection).await {
            Ok(found) => observations.mounted.extend(found),
            // 源目录已经不存在（容器还在、目录被删）：没有可保护的东西
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
            Err(error) => {
                observations.unidentifiable = Some(format!(
                    "mount source {} cannot be identified from inside RCoder: {error}",
                    source.display()
                ));
                break;
            }
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
    identities: &dyn DirectoryIdentities,
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
    let identity = match identities.identity(candidate).await {
        Ok(identity) => identity,
        // 列目录之后目录已经消失，没有留下可被挂载的实体
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
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
    let identities = FileSystemIdentities;
    let observations = observe(&space, &sources, &identities).await;

    let mut mounts = LogDirectoryMounts::default();
    for candidate in candidates {
        match classify(candidate, &space, &observations, &identities).await {
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
