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
//! （`DockerManagerConfig::log_reclaim_root`）下面的都是。见 `agent_container_starter::mounts`。
//!
//! # 正在准备挂载的目录
//!
//! 目录建好到 Docker 登记这个挂载之间（中间可能隔着一次很慢的镜像拉取，多久没有上界），它在
//! 挂载清单里还看不到。这段时间靠 [`pending`] 按路径登记，清理器把登记着的目录当作仍被挂载。

mod domain;
mod pending;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use tracing::warn;

/// 同名目录已存在时，名字最多试到 `<名字>-16`
pub(crate) const NAME_ATTEMPTS: u32 = 16;

pub(crate) use domain::LogDomain;
pub use pending::PendingBinds;
pub(crate) use pending::pending_bind_sources;

/// 仅供其它 crate 的测试使用：登记一条正在准备挂载的宿主机路径，守卫 drop 时注销。需要启用
/// `test-support` feature，生产构建里不存在。
#[cfg(feature = "test-support")]
pub fn claim_pending_bind_for_test(host_path: &Path) -> PendingBinds {
    let mut binds = PendingBinds::default();
    binds.claim(host_path);
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
