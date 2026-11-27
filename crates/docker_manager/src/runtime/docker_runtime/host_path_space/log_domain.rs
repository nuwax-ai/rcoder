//! 可回收的日志域：哪些 bind 源落在日志清理器回收的日志根之下。
//!
//! 日志清理器回收**可回收的日志根**（`DockerManagerConfig::log_reclaim_root`，和清理器的
//! `cleanup_config.log_cleanup.log_dir` 是同一个目录）下面过期且没有被挂载的目录。所以落在它
//! 下面的 bind 源，不管容器内的挂载点叫什么，都必须是为本容器独占新建的目录
//! （`crate::log_directory`）：绑定一个已有的目录，就可能和回收撞上。
//!
//! # 怎么判
//!
//! Docker 登记挂载源时做词法归一化（`/a/b/../c` 就是 `/a/c`），挂载那一刻由内核在宿主机上解析
//! 路径上的符号链接。所以一个 bind 源判两次，用的都是清理器那个路径空间
//! （[`super::HostPathSpace`]）：
//!
//! 1. 词法：归一化之后看是不是在日志根之下；
//! 2. 真实位置：逐级解析符号链接之后（[`real_location`]）再看一次，日志根也按同样办法解析。
//!
//! 任一次"在"就是在。两次都"不在"才是确实不在。
//!
//! # 判不出来
//!
//! 日志根只定出词法位置（真实位置求不出来）、连路径空间都取不到、或者某条 bind 源的真实位置
//! 求不出来（走进 RCoder 看不见的路径、被另一条挂载遮蔽）时，这条源和日志根的关系判不出来
//! （[`LogDomainVerdict::Unknown`]）。
//!
//! 判出来是什么就按什么执行；判不出来**不拒绝创建容器**——那会让一个环境问题变成所有容器都
//! 起不来。调用方照常挂载，把这条源登记到创建结束（`crate::log_directory::PendingBinds`）。
//! 这样是安全的：清理器求真实位置用的是同一个走法，它对这条源同样判不出来，而它确认不了
//! 就不删（整轮所有候选都保留）。

use std::path::{Path, PathBuf};

use tokio::sync::OnceCell;

use super::real_location::{LocalFileSystem, RealLocations};
use super::{HostPathSpace, viewing};
use crate::{DockerManager, HostPathResolver};

/// 可回收的日志根，以及怎么判断一条 bind 源在不在它下面
#[derive(Debug)]
pub(crate) enum LogDomain {
    /// 日志根的宿主机路径定出来了；真实位置可能求不出来
    Root {
        /// 归一化之后的宿主机路径
        lexical: PathBuf,
        /// 逐级解析符号链接之后的宿主机路径；求不出来时是原因
        real: Result<PathBuf, String>,
    },
    /// 什么都判不出来：取不到路径空间，日志根不是绝对路径，或者 RCoder 自己的挂载没有一条
    /// 覆盖它
    Blind(String),
}

/// 一条 bind 源和日志根的关系
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum LogDomainVerdict {
    /// 在日志根之下：必须是为本容器独占新建的目录
    Inside,
    /// 确认不在
    Outside,
    /// 判不出来。调用方照常处理这条源，但要把它登记到创建结束，不能当成"不在"
    Unknown(String),
}

impl LogDomain {
    /// 什么都判不出来，`reason` 说明缺什么
    pub(crate) fn blind(reason: impl Into<String>) -> Self {
        Self::Blind(reason.into())
    }

    /// 定位可回收的日志根（RCoder 进程里的路径）。定位到哪一步算哪一步，不报错：
    ///
    /// - `space` 取不到（`None`）：什么都判不出来；
    /// - 日志根不是绝对路径：清理器按工作目录读它，这里对不上，什么都判不出来；
    /// - 路径空间里没有挂载覆盖日志根：同样什么都判不出来。换算失败只说明词法路径没命中
    ///   挂载表，清理器照样能从这个进程路径读到目录（比如它是指向某个已映射目录的符号链接）；
    /// - 宿主机路径定出来了、真实位置求不出来：只有词法判得出。
    pub(crate) async fn locate(space: Option<&HostPathSpace<'_>>, reclaim_root: &Path) -> Self {
        let Some(space) = space else {
            return Self::Blind("RCoder's own mount table is unavailable".to_owned());
        };
        if !reclaim_root.is_absolute() {
            return Self::Blind(format!(
                "the reclaimable log root {} is not an absolute path",
                reclaim_root.display()
            ));
        }
        let Ok(lexical) = space.to_host(reclaim_root) else {
            return Self::Blind(format!(
                "no mount of RCoder's own container covers the reclaimable log root {}",
                reclaim_root.display()
            ));
        };
        let lexical = crate::normalize_path(&lexical);
        let real = RealLocations::new(space, &LocalFileSystem)
            .real_location(&lexical)
            .await;
        Self::Root { lexical, real }
    }

    /// 这次判定缺什么：取不到路径空间，或者日志根的真实位置求不出来。不缺时为 `None`
    pub(crate) fn limitation(&self) -> Option<String> {
        match self {
            Self::Blind(reason) => Some(reason.clone()),
            Self::Root {
                real: Err(reason), ..
            } => Some(reason.clone()),
            Self::Root { real: Ok(_), .. } => None,
        }
    }

    /// 一条 bind 源（宿主机路径）在不在日志根之下。`space` 是定位时用的那个路径空间
    pub(crate) async fn classify(
        &self,
        space: Option<&HostPathSpace<'_>>,
        host_path: &str,
    ) -> LogDomainVerdict {
        match self {
            Self::Blind(reason) => LogDomainVerdict::Unknown(reason.clone()),
            Self::Root { lexical, real } => {
                let lexical_path = crate::normalize_path(Path::new(host_path));
                if strictly_under(&lexical_path, lexical) {
                    return LogDomainVerdict::Inside;
                }
                let (Ok(real), Some(space)) = (real, space) else {
                    let reason = match real {
                        Err(reason) => format!(
                            "the real location of the reclaimable log root is unknown: {reason}"
                        ),
                        Ok(_) => "RCoder's own mount table is unavailable".to_owned(),
                    };
                    return LogDomainVerdict::Unknown(reason);
                };
                match RealLocations::new(space, &LocalFileSystem)
                    .real_location(&lexical_path)
                    .await
                {
                    Ok(path) if strictly_under(&path, real) => LogDomainVerdict::Inside,
                    Ok(_) => LogDomainVerdict::Outside,
                    Err(reason) => LogDomainVerdict::Unknown(reason),
                }
            }
        }
    }
}

/// `path` 严格位于 `root` 之下。`root` 本身不算：清理器回收的是它里面的目录
fn strictly_under(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root)
        .is_ok_and(|under| !under.as_os_str().is_empty())
}

/// 一次操作（创建容器、builder 重启、组装应用挂载）对可回收日志根的了解：清理器那个路径
/// 空间，加上在它里面定位出来的日志域。
///
/// 取不到路径空间也不报错——那会让所有容器都起不来。这时什么都判不出来
/// （[`LogDomainVerdict::Unknown`]），调用方照常处理每条挂载，把判不出来的源登记到操作结束。
/// [`Self::limitation`] 说明缺什么，由 [`Self::warn_if_limited`] 告警。
pub(crate) struct LogDomainAdmission<'a> {
    space: Option<HostPathSpace<'a>>,
    domain: LogDomain,
}

impl<'a> LogDomainAdmission<'a> {
    /// 取路径空间、定位日志根（`reclaim_root` 是 RCoder 进程里的路径）
    pub(crate) async fn locate(
        manager: &DockerManager,
        resolver: &'a OnceCell<HostPathResolver>,
        reclaim_root: &Path,
    ) -> Self {
        match super::path_space(manager, resolver).await {
            Ok(space) => Self {
                domain: LogDomain::locate(Some(&space), reclaim_root).await,
                space: Some(space),
            },
            Err(error) => Self {
                domain: LogDomain::blind(format!(
                    "RCoder's own mount table is unavailable: {error}"
                )),
                space: None,
            },
        }
    }

    /// 测试用：已经有路径空间时，在它里面定位日志根
    #[cfg(test)]
    pub(crate) async fn in_space(space: HostPathSpace<'a>, reclaim_root: &Path) -> Self {
        Self {
            domain: LogDomain::locate(Some(&space), reclaim_root).await,
            space: Some(space),
        }
    }

    /// 这次判定缺什么：取不到路径空间，或者日志根的真实位置求不出来。不缺时为 `None`
    pub(crate) fn limitation(&self) -> Option<String> {
        self.domain.limitation()
    }

    /// 定位时用的那个路径空间；取不到时为 `None`
    pub(crate) fn space(&self) -> Option<&HostPathSpace<'a>> {
        self.space.as_ref()
    }

    /// 一条 bind 源（宿主机路径）在不在日志根之下。`what` 说明是哪条挂载（容器内路径），记
    /// 日志用。
    ///
    /// 判不出来时记一笔：这次判定整体就不完整的话（日志根定位不出来、没有路径空间）开头已经
    /// 告警过，只记 debug；否则是这条源自己的问题（符号链接走到 RCoder 看不见的地方、被另一条
    /// 挂载遮蔽），告警。调用方对判不出来的源照常处理，但要把它登记到操作结束。
    pub(crate) async fn classify(&self, what: &str, host_path: &str) -> LogDomainVerdict {
        let verdict = self.domain.classify(self.space(), host_path).await;
        if let LogDomainVerdict::Unknown(reason) = &verdict {
            if self.limitation().is_some() {
                tracing::debug!(
                    mount = what,
                    source = host_path,
                    %reason,
                    "Mount cannot be classified against the reclaimable log root; it is registered as in use until the operation finishes"
                );
            } else {
                tracing::warn!(
                    mount = what,
                    source = host_path,
                    %reason,
                    "Cannot tell whether this mount lies under the reclaimable log root; it is used as configured and registered as in use until the operation finishes"
                );
            }
        }
        verdict
    }

    /// 判定不完整时告警一次：哪些挂载落在日志根里没有完全判出来，判不出来的照配置挂载并登记
    pub(crate) fn warn_if_limited(&self, operation: &str) {
        if let Some(reason) = self.limitation() {
            tracing::warn!(
                operation,
                %reason,
                "Cannot fully tell which mounts lie under the reclaimable log root; mounts that cannot be classified are used as configured and registered as in use until the operation finishes, instead of refusing it"
            );
        }
    }
}

/// 一条 `源:挂载点[:选项]` 形式的 bind 里的源。和 Docker 的切分一致：以 `/` 开头的才是宿主机
/// 路径，取到第一个冒号为止；命名卷、Windows 路径不是宿主机路径，返回 `None`
pub(crate) fn bind_source(bind: &str) -> Option<&str> {
    bind.starts_with('/')
        .then(|| bind.split(':').next())
        .flatten()
}

/// 在宿主机路径 `host_dir` 上新建一个空目录，经 RCoder 自己的挂载视图操作，返回新建目录在
/// RCoder 进程里的路径。
///
/// 新建是独占的（见 `crate::log_directory`）：同名目录已经存在时换一个名字，所以返回路径的
/// 最后一级不一定等于 `host_dir` 的。
pub(crate) async fn create_host_directory(
    space: &HostPathSpace<'_>,
    host_dir: &Path,
) -> Result<PathBuf, CreateHostDirectoryError> {
    let (Some(parent), Some(name)) = (
        host_dir.parent(),
        host_dir.file_name().and_then(|name| name.to_str()),
    ) else {
        return Err(CreateHostDirectoryError::NotVisible);
    };
    let visible = viewing(space, parent)
        .visible_path()
        .ok_or(CreateHostDirectoryError::NotVisible)?;
    crate::log_directory::create_fresh_directory(&visible, name)
        .await
        .map_err(CreateHostDirectoryError::Create)
}

/// 在宿主机上建目录失败的原因
#[derive(Debug)]
pub(crate) enum CreateHostDirectoryError {
    /// RCoder 的挂载看不到这个位置：在所有挂载之外，或者被另一条挂载遮蔽
    NotVisible,
    /// 看得到，但目录没建成
    Create(crate::log_directory::FreshDirectoryError),
}

impl std::fmt::Display for CreateHostDirectoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotVisible => write!(
                f,
                "no mount of RCoder's own container shows this host directory"
            ),
            Self::Create(error) => write!(f, "{error}"),
        }
    }
}
