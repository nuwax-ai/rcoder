//! 容器日志目录一次性使用
//!
//! 每个容器的日志目录只在"建目录 → Docker 登记这个挂载"那一刻被绑定一次，之后不再被任何
//! 新容器复用：agent 容器启动时新建一个（`agent_container_starter::mounts`），builder 重启出
//! 替代容器时也新建一个（`runtime::docker_builder_restart`）。日志清理器凭这一点只看"目录
//! 已过期、清单里没有容器挂着它"就回收，不必和挂载准备互斥：清理器手里的候选都是旧目录，
//! 正在准备挂载的都是刚建的目录，两边没有交集。
//!
//! "新建"必须是真的新建：[`create_fresh_directory`] 用独占创建建目录，同名目录已经存在
//! （时钟回拨到同一秒、同名容器的并发创建）时换一个名字，不复用。旧目录可能已经过期、正被
//! 清理器回收。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use tracing::warn;

/// 同名目录已存在时，名字最多试到 `<名字>-16`
pub(crate) const NAME_ATTEMPTS: u32 = 16;

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
    let mut name = preferred.to_owned();
    for attempt in 2..=NAME_ATTEMPTS {
        let path = parent.join(&name);
        match tokio::fs::create_dir(&path).await {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                warn!(
                    directory = %path.display(),
                    "Log directory name is already taken; binding a new directory under another name instead of reusing it"
                );
                name = format!("{preferred}-{attempt}");
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
