//! 日志目录的挂载判定：一个候选目录是否仍被某个容器当挂载源。
//!
//! # 两边必须在同一个路径空间里比
//!
//! 创建容器时，`apply_auto_mounts` 把容器内的日志路径按 RCoder 自身的挂载表做**词法**前缀
//! 替换，得到的宿主机路径原样交给 Docker；Docker 清单里的 `Source` 也是这个词法路径。
//! 所以第一步必须用一模一样的推导去比。只对候选解析符号链接再去比，会让一个仍在挂载的
//! 目录被判成未挂载而被删掉。
//!
//! 光有词法比对还不够：同一个物理目录可以有两个名字。候选走真实路径、容器经别名登记
//! （或者反过来）时，两边的字符串对不上。所以再把清单里的挂载源反向换算回 RCoder 的
//! 命名空间，解析出物理路径，用物理身份比第二次。
//!
//! # 判定规则
//!
//! 1. 候选按词法换算到宿主机后，等于清单里的某个挂载源或位于它之下 → 仍被挂载；
//! 2. 候选的物理路径等于某个挂载源的物理路径，或位于它之下 → 仍被挂载；
//! 3. 候选无法换算、或无法解析出物理路径 → 无法确认，保留；
//! 4. 其余 → 确认未被挂载。
//!
//! 残余边界：容器形态下，只有落在 RCoder 自身挂载之下的挂载源才能反向换算。第三方容器
//! 经由 RCoder 看不到的宿主机别名挂载同一个目录，从容器内识别不了。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

impl HostPathSpace<'_> {
    /// 与创建侧 `apply_auto_mounts` 登记挂载时逐字一致：先做词法归一化，再替换前缀
    fn to_host(&self, path: &Path) -> DockerResult<PathBuf> {
        let lexical = crate::normalize_path(path);
        match self {
            HostPathSpace::Host => Ok(lexical),
            HostPathSpace::Container(resolver) => resolver.resolve_to_host_path(&lexical),
        }
    }

    fn to_process(&self, host_path: &Path) -> Option<PathBuf> {
        match self {
            HostPathSpace::Host => Some(crate::normalize_path(host_path)),
            HostPathSpace::Container(resolver) => resolver
                .resolve_from_host_path(host_path)
                .map(|path| crate::normalize_path(&path)),
        }
    }
}

/// 候选换算不到宿主机时的原因
fn mapping_failure(error: &DockerError) -> String {
    format!("Map a log directory onto the host: {error}")
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
    registered: &HashSet<PathBuf>,
    physical: &HashSet<PathBuf>,
) -> LogDirMount {
    // 与创建侧完全相同的推导
    let registered_path = match space.to_host(candidate) {
        Ok(path) => crate::normalize_path(&path),
        Err(error) => return LogDirMount::Unconfirmed(mapping_failure(&error)),
    };
    if registered.contains(&registered_path) {
        return LogDirMount::Mounted;
    }

    let physical_path = match tokio::fs::canonicalize(candidate).await {
        Ok(path) => crate::normalize_path(&path),
        // 列目录之后目录已经消失，没有留下可被挂载的实体
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LogDirMount::Unmounted;
        }
        Err(error) => {
            return LogDirMount::Unconfirmed(format!(
                "it cannot be resolved to a physical path: {error}"
            ));
        }
    };

    // 同一个物理目录可能以别名登记：物理路径落在清单的物理路径之下同样算被挂载
    if physical.contains(&physical_path) {
        return LogDirMount::Mounted;
    }
    // 候选路径经过符号链接时，再按物理路径的宿主机形式比对一次。反向换算不了的挂载源
    // （不在 RCoder 自身挂载之下）只有这一种比法能对上。
    if physical_path != crate::normalize_path(candidate)
        && let Ok(host_path) = space.to_host(&physical_path)
        && registered.contains(&crate::normalize_path(&host_path))
    {
        return LogDirMount::Mounted;
    }
    LogDirMount::Unmounted
}

/// 清单里的挂载源换算回 RCoder 的命名空间、解析出物理路径之后，连同祖先的集合。
///
/// 经别名登记的挂载只有在这里才能和候选目录对上。换算不回来（不在 RCoder 自身挂载之下）
/// 或在进程里解析不了的挂载源不进入这个集合，它们只参与词法比对。
async fn physical_mount_points(space: &HostPathSpace<'_>, sources: &[PathBuf]) -> HashSet<PathBuf> {
    let mut physical = HashSet::new();
    for source in sources {
        if let Some(visible) = space.to_process(source)
            && let Ok(resolved) = tokio::fs::canonicalize(&visible).await
        {
            physical.extend(resolved.ancestors().map(Path::to_path_buf));
        }
    }
    physical
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
/// 清单一律取自真实 Docker，不读进程内缓存，整轮只发一次列表请求。单个候选换算或解析
/// 失败只影响它自己，记为无法确认；只有整份清单拿不到时才返回 `Err`。
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
    let registered = with_ancestors(sources.iter().cloned());

    let space = match log_path_resolver.get() {
        // 显式注入的挂载表优先：调用方已经知道映射关系，不再自检
        Some(resolver) => HostPathSpace::Container(resolver),
        None if shared_types::is_deploy_host() => HostPathSpace::Host,
        None => HostPathSpace::Container(container_resolver(manager, log_path_resolver).await?),
    };
    let physical = physical_mount_points(&space, &sources).await;

    let mut mounts = LogDirectoryMounts::default();
    for candidate in candidates {
        match classify(candidate, &space, &registered, &physical).await {
            LogDirMount::Mounted => {
                mounts.mounted.insert(candidate.clone());
            }
            LogDirMount::Unmounted => {}
            LogDirMount::Unconfirmed(reason) => {
                mounts.unconfirmed.push((candidate.clone(), reason));
            }
        }
    }
    Ok(mounts)
}

#[cfg(test)]
mod tests;
