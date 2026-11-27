//! RCoder 进程命名空间里的路径如何与宿主机路径互相换算。
//!
//! 几处共用这一套视图，看到的必须是同一个映射：判定日志目录是否仍被挂载
//! （[`super::log_dir_mounts`]）、判断一条 bind 源在不在可回收的日志根里（[`log_domain`]，
//! 创建容器、builder 重启、组装应用数据挂载都用它）、给替代 builder 新建日志目录。
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

mod log_domain;
mod real_location;

#[cfg(test)]
pub(crate) use log_domain::LogDomain;
pub(crate) use log_domain::{
    LogDomainAdmission, LogDomainVerdict, bind_source, create_host_directory,
};
#[cfg(test)]
pub(super) use real_location::Entry;
pub(super) use real_location::{DirectoryId, FileSystemView, is_absent};
pub(in crate::runtime) use real_location::{LocalFileSystem, Location, RealLocations};

use std::path::{Path, PathBuf};
use std::time::Duration;

use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult};
use tokio::sync::OnceCell;

use crate::{DockerManager, HostPathResolver};

/// RCoder 进程命名空间里的路径如何与宿主机路径互相换算
pub(crate) enum HostPathSpace<'a> {
    /// deploy-host 形态：RCoder 与 daemon 同在宿主机，进程路径就是宿主机路径
    Host,
    /// 容器形态：按 RCoder 所在容器的挂载表替换前缀
    Container(&'a HostPathResolver),
}

/// 一个宿主机路径在 RCoder 命名空间里的一个视图：这个宿主机路径经某条挂载看到的路径
pub(crate) struct Projection {
    path: PathBuf,
}

impl HostPathSpace<'_> {
    /// 与创建侧 `apply_auto_mounts` 登记挂载时逐字一致：先做词法归一化，再替换前缀
    pub(crate) fn to_host(&self, path: &Path) -> crate::DockerResult<PathBuf> {
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

/// 一个宿主机路径经哪个视图看得到
pub(crate) enum Viewed {
    Through(Projection),
    /// RCoder 的任何挂载都不覆盖它
    Unmounted,
    /// 有挂载覆盖，但每个视图都对不上：这个位置被另一条挂载遮蔽了
    Hidden,
}

impl Viewed {
    /// 这个视图在 RCoder 进程里的路径；RCoder 看不到时为 `None`
    pub(crate) fn visible_path(self) -> Option<PathBuf> {
        match self {
            Viewed::Through(projection) => Some(projection.path),
            Viewed::Unmounted | Viewed::Hidden => None,
        }
    }
}

/// 取最具体的那个能核验的视图。视图里的路径要能换算回这个宿主机路径才算数：路径中途被
/// 另一条挂载遮蔽时，看到的是另一棵目录，换算回去就对不上
pub(crate) fn viewing(space: &HostPathSpace<'_>, host_path: &Path) -> Viewed {
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

/// 这次用哪个路径空间——清理器看宿主机路径的那一副眼镜。
///
/// 显式注入的挂载表优先；deploy-host 形态进程路径就是宿主机路径；否则自检 RCoder 所在容器的
/// 挂载表（每个 `log_path_resolver` 只做一次）。
///
/// 取不到路径空间是"信息不足"，不是"没有日志域"：清理器这时保留所有候选；创建容器、builder
/// 重启不因此失败，见 [`log_domain::LogDomainAdmission`]。
pub(crate) async fn path_space<'a>(
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

#[cfg(test)]
mod tests;
