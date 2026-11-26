//! RCoder 进程命名空间里的路径如何与宿主机路径互相换算。
//!
//! 判定日志目录是否仍被挂载（[`super::log_dir_mounts`]）和给替代 builder 新建日志目录
//! （[`create_host_directory`]）共用这一套换算，两边看到的必须是同一个映射。
//!
//! # 两边必须在同一个路径空间里比
//!
//! 创建容器时，`apply_auto_mounts` 把容器内的日志路径按 RCoder 自身的挂载表做**词法**前缀
//! 替换，得到的宿主机路径原样交给 Docker；Docker 清单里的 `Source` 也是这个词法路径。
//! 所以换算必须用一模一样的推导。
//!
//! # 视图要能核验
//!
//! [`viewing`] 只采纳能核验的视图：视图里的路径换算回宿主机必须还是这个路径。容器内的
//! 路径被另一条挂载遮蔽时，看到的是另一棵目录，换算回去就对不上，这样的视图不算。

use std::path::{Path, PathBuf};
use std::time::Duration;

use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult};
use tokio::sync::OnceCell;

use crate::{DockerManager, DockerResult, HostPathResolver};

/// RCoder 进程命名空间里的路径如何与宿主机路径互相换算
pub(super) enum HostPathSpace<'a> {
    /// deploy-host 形态：RCoder 与 daemon 同在宿主机，进程路径就是宿主机路径
    Host,
    /// 容器形态：按 RCoder 所在容器的挂载表替换前缀
    Container(&'a HostPathResolver),
}

/// 一个宿主机路径在 RCoder 命名空间里的一个视图：这个宿主机路径经某条挂载看到的路径
pub(super) struct Projection {
    pub(super) path: PathBuf,
}

impl HostPathSpace<'_> {
    /// 与创建侧 `apply_auto_mounts` 登记挂载时逐字一致：先做词法归一化，再替换前缀
    pub(super) fn to_host(&self, path: &Path) -> DockerResult<PathBuf> {
        let lexical = crate::normalize_path(path);
        match self {
            HostPathSpace::Host => Ok(lexical),
            HostPathSpace::Container(resolver) => resolver.resolve_to_host_path(&lexical),
        }
    }

    /// `host_path` 在 RCoder 命名空间里的全部视图：同一个宿主机目录挂了几次就有几个
    pub(super) fn projections(&self, host_path: &Path) -> Vec<Projection> {
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
    pub(super) fn shows(&self, projection: &Projection, host_path: &Path) -> bool {
        self.to_host(&projection.path)
            .is_ok_and(|mapped| mapped == crate::normalize_path(host_path))
    }
}

/// 一个宿主机路径经哪个视图看得到
pub(super) enum Viewed {
    Through(Projection),
    /// RCoder 的任何挂载都不覆盖它
    Unmounted,
    /// 有挂载覆盖，但每个视图都对不上：这个位置被另一条挂载遮蔽了
    Hidden,
}

/// 取最具体的那个能核验的视图。视图里的路径要能换算回这个宿主机路径才算数：路径中途被
/// 另一条挂载遮蔽时，看到的是另一棵目录，换算回去就对不上
pub(super) fn viewing(space: &HostPathSpace<'_>, host_path: &Path) -> Viewed {
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

/// 在宿主机路径 `host_dir` 上新建一个空目录，经 RCoder 自己的挂载视图操作，返回新建目录在
/// RCoder 进程里的路径。
///
/// 新建是独占的（见 `crate::log_directory`）：同名目录已经存在时换一个名字，所以返回路径的
/// 最后一级不一定等于 `host_dir` 的。
pub(crate) async fn create_host_directory(
    manager: &DockerManager,
    log_path_resolver: &OnceCell<HostPathResolver>,
    host_dir: &Path,
) -> Result<PathBuf, CreateHostDirectoryError> {
    let space = path_space(manager, log_path_resolver)
        .await
        .map_err(|error| CreateHostDirectoryError::Resolve(error.to_string()))?;
    let (Some(parent), Some(name)) = (
        host_dir.parent(),
        host_dir.file_name().and_then(|name| name.to_str()),
    ) else {
        return Err(CreateHostDirectoryError::NotVisible);
    };
    let Viewed::Through(projection) = viewing(&space, parent) else {
        return Err(CreateHostDirectoryError::NotVisible);
    };
    crate::log_directory::create_fresh_directory(&projection.path, name)
        .await
        .map_err(CreateHostDirectoryError::Create)
}

/// 在宿主机上建目录失败的原因
#[derive(Debug)]
pub(crate) enum CreateHostDirectoryError {
    /// RCoder 所在容器的挂载表取不到：自检失败或超时
    Resolve(String),
    /// RCoder 的挂载看不到这个位置：在所有挂载之外，或者被另一条挂载遮蔽
    NotVisible,
    /// 看得到，但目录没建成
    Create(crate::log_directory::FreshDirectoryError),
}

impl std::fmt::Display for CreateHostDirectoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resolve(detail) => write!(f, "RCoder's own mount table is unavailable: {detail}"),
            Self::NotVisible => write!(
                f,
                "no mount of RCoder's own container shows this host directory"
            ),
            Self::Create(error) => write!(f, "{error}"),
        }
    }
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

/// 这次用哪个路径空间：显式注入的挂载表优先，其次 deploy-host，否则自检 RCoder 所在容器的
/// 挂载表（只在首次用到时做一次）
pub(super) async fn path_space<'a>(
    manager: &DockerManager,
    log_path_resolver: &'a OnceCell<HostPathResolver>,
) -> ContainerRuntimeResult<HostPathSpace<'a>> {
    Ok(match log_path_resolver.get() {
        // 显式注入的挂载表优先：调用方已经知道映射关系，不再自检
        Some(resolver) => HostPathSpace::Container(resolver),
        None if shared_types::is_deploy_host() => HostPathSpace::Host,
        None => HostPathSpace::Container(container_resolver(manager, log_path_resolver).await?),
    })
}
