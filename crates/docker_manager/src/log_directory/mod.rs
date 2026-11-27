//! 容器日志目录一次性使用
//!
//! 每个容器的日志目录只在"建目录 → Docker 登记这个挂载"那一刻被绑定一次，之后不再被任何
//! 新容器复用：agent 容器启动时新建一个（`agent_container_starter::mounts`），builder 重启出
//! 替代容器时也新建一个（`runtime::docker_builder_restart`）。日志清理器据此只看三件事就
//! 回收：目录已过期、清单里没有容器挂着它、也没有人正在准备挂它。它不必和挂载准备互斥。
//!
//! "新建"必须是真的新建：[`create_fresh_directory`] 用独占创建建目录，同名目录已经存在
//! （时钟回拨到同一秒、同名容器的并发创建）时换一个名字，不复用。旧目录可能已经过期、正被
//! 清理器回收。建不出来就是建不出来，不退回去绑定任何已有的路径。
//!
//! 哪些 bind 源必须走这一套，按**源路径**判定，不看容器内的挂载点叫什么：落在可回收的日志根
//! （`DockerManagerConfig::log_reclaim_root`）下面的都是。怎么判在
//! `runtime::docker_runtime::host_path_space::LogDomain`，创建容器和 builder 重启都用它。
//!
//! # 正在准备挂载的目录
//!
//! 目录建好到 Docker 登记这个挂载之间（中间可能隔着一次很慢的镜像拉取，多久没有上界），它在
//! 挂载清单里还看不到。这段时间靠 [`pending`] 按路径登记，清理器把登记着的目录当作仍被挂载。
//! 登记分两类：创建还在进行的（在途，不淘汰），和请求发出去了却没能确认结果的（结果不明，
//! 保留到过期）。
//!
//! 登记的不只是新建的日志目录：创建方判不出在不在日志根里的 bind 源也登记进来（靠的是同一个
//! 路径空间，`runtime::docker_runtime::host_path_space`）。判不出来不拒绝创建容器——清理器对这条
//! 源同样判不出来，登记着它就整轮不删。
//!
//! 清理器一轮只读一次登记和 Docker 清单，读不到它读完之后才发生的创建。所以删每个目录之前它
//! 还要回来确认一次（[`mark_reclaim`]）：先打上"正在删"的标记，再核对——有创建方登记着它或它
//! 下面的路径，就解除标记、留到下一轮；清单之后有创建开始或结束，就在标记之下重新取一次 Docker
//! 清单；都没有才删。标记在，这之后才登记它的创建方就会等这次删除结束，最多 10 秒。见
//! [`pending`]。
//!
//! # 哪些挂载是 RCoder 建的
//!
//! 创建容器时独占新建了目录的挂载，记在容器的 label 上（[`record`]）。之后重启这个容器时，
//! 只有记录里的挂载才换成新目录。名字像不像日志目录不算数：带数据的目录也可以叫
//! `<什么>-<时间戳>`。
//!
//! 新建目录落在日志根下面更深处时，要刷新上一级候选的修改时间，见
//! [`refresh_candidate_ancestor`]。

mod pending;
mod record;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use tracing::warn;

/// 同名目录已存在时，名字最多试到 `<名字>-16`
pub(crate) const NAME_ATTEMPTS: u32 = 16;

#[cfg(test)]
pub(crate) use pending::pending_bind_sources;
pub use pending::{DirectoryBeingReclaimed, PendingBinds};
pub(crate) use pending::{
    mark_reclaim, reclaim_marks, reclaim_released, registry_view, unresolved_sources,
};
pub(crate) use record::{LOG_MOUNTS_LABEL, LogMountRecord, decode_log_mounts, encode_log_mounts};

/// 仅供其它 crate 的测试使用：登记一条正在准备挂载的宿主机路径，守卫 drop 时注销。需要启用
/// `test-support` feature，生产构建里不存在。
#[cfg(feature = "test-support")]
pub async fn claim_pending_bind_for_test(host_path: &Path) -> PendingBinds {
    let mut binds = PendingBinds::default();
    binds
        .claim(host_path, None)
        .await
        .expect("a test claim must not wait for a reclaim");
    binds
}

/// 日志目录名：`<容器名>-<UTC 时间>`，精确到秒
pub(crate) fn log_directory_name(owner: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    format!("{}-{}", owner, now.format("%Y%m%d%H%M%S"))
}

/// 替代容器的日志目录名：沿用原目录名里时间戳之前的部分，换上新的时间
pub(crate) fn replacement_log_directory_name(
    previous: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    match previous.rsplit_once('-') {
        Some((owner, timestamp)) if is_timestamp(timestamp) => log_directory_name(owner, now),
        _ => log_directory_name(previous, now),
    }
}

fn is_timestamp(text: &str) -> bool {
    text.len() == 14 && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// 在 `parent` 下新建一个目录，名字优先用 `preferred`，返回新建目录的路径。
///
/// `parent` 不存在时一并建出来。同名目录已经存在时不算成功：换一个名字再建，宁可多建一个
/// 目录，也不把一个来路不明的目录绑给新容器。
pub(crate) async fn create_fresh_directory(
    parent: &Path,
    preferred: &str,
) -> Result<PathBuf, FreshDirectoryError> {
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(FreshDirectoryError::Io)?;
    // 首选名字被占，依次换 -2、-3 …：连同首选一共试 NAME_ATTEMPTS 个
    for attempt in 0..NAME_ATTEMPTS {
        let name = if attempt == 0 {
            preferred.to_owned()
        } else {
            format!("{preferred}-{}", attempt + 1)
        };
        let path = parent.join(&name);
        match tokio::fs::create_dir(&path).await {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                warn!(
                    directory = %path.display(),
                    "Log directory name is already taken; binding a new directory under another name instead of reusing it"
                );
            }
            Err(error) => return Err(FreshDirectoryError::Io(error)),
        }
    }
    Err(FreshDirectoryError::Exhausted)
}

/// 新建目录落在日志根下面第 2 层或更深时，把它在日志根下的第一级目录的修改时间刷新到现在。
///
/// 清理器只扫日志根的第一层，过期的是那第一级目录。在它下面更深处新建目录不会改它的修改
/// 时间，它一直是过期候选。登记能护住它，但清理器要先读到登记才看得见；刷新之后，清理器删
/// 之前那一次复查（看目录还在、仍然过期）就会看到"变了"，这一轮不动它。
///
/// 只在新建目录按词法就在日志根下面时做。刷新失败只记日志：登记照样有效
pub(crate) async fn refresh_candidate_ancestor(created: &Path, reclaim_root: &Path) {
    let Some(ancestor) = candidate_ancestor(created, reclaim_root) else {
        return;
    };
    let refreshed = async {
        let file = tokio::fs::File::open(&ancestor).await?;
        file.set_modified(std::time::SystemTime::now()).await
    }
    .await;
    if let Err(error) = refreshed {
        warn!(
            directory = %ancestor.display(),
            %error,
            "Cannot refresh the modification time of the directory holding a new log directory; the log cleaner may see it as expired until its next scan"
        );
    }
}

/// `created` 在日志根下面第 2 层或更深时，它在日志根下的第一级目录
fn candidate_ancestor(created: &Path, reclaim_root: &Path) -> Option<PathBuf> {
    let created = crate::normalize_path(created);
    let reclaim_root = crate::normalize_path(reclaim_root);
    let mut below = created.strip_prefix(&reclaim_root).ok()?.components();
    let first = below.next()?;
    // 第二层往下才有"上层候选"：第一层的新建目录自己是新目录，不会是过期候选
    below.next()?;
    Some(reclaim_root.join(first))
}

/// 新建日志目录失败的原因
#[derive(Debug)]
pub(crate) enum FreshDirectoryError {
    /// 从 `preferred` 到 `preferred-16` 每个名字都已经被占用，换无可换
    Exhausted,
    /// 建目录时出错：上级目录建不出来、没有权限……
    Io(std::io::Error),
}

impl std::fmt::Display for FreshDirectoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exhausted => write!(
                f,
                "every name up to -{NAME_ATTEMPTS} is already taken under this parent"
            ),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

#[cfg(test)]
mod tests;
