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
mod tests {
    use super::*;

    fn at(text: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn a_log_directory_is_named_after_its_container_and_the_utc_second() {
        assert_eq!(
            log_directory_name("rcoder-app-builder-265", at("2025-10-26T08:00:00Z")),
            "rcoder-app-builder-265-20251026080000"
        );
    }

    #[tokio::test]
    async fn a_fresh_directory_is_created_under_its_preferred_name() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("logs").join("container");

        let created = create_fresh_directory(&parent, "runner-20251026080000")
            .await
            .unwrap();

        assert_eq!(created, parent.join("runner-20251026080000"));
        assert!(created.is_dir());
    }

    #[tokio::test]
    async fn an_existing_directory_is_never_reused() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("runner-20251026080000")).unwrap();
        std::fs::write(root.path().join("runner-20251026080000-2"), b"a file").unwrap();

        let created = create_fresh_directory(root.path(), "runner-20251026080000")
            .await
            .unwrap();

        assert_eq!(created, root.path().join("runner-20251026080000-3"));
        assert!(root.path().join("runner-20251026080000").is_dir());
        assert_eq!(
            std::fs::read(root.path().join("runner-20251026080000-2")).unwrap(),
            b"a file"
        );
    }

    #[tokio::test]
    async fn every_taken_name_is_reported_instead_of_reusing_one() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("runner")).unwrap();
        for attempt in 2..=NAME_ATTEMPTS {
            std::fs::create_dir(root.path().join(format!("runner-{attempt}"))).unwrap();
        }

        let error = create_fresh_directory(root.path(), "runner")
            .await
            .unwrap_err();

        assert!(matches!(error, FreshDirectoryError::Exhausted));
    }
}
