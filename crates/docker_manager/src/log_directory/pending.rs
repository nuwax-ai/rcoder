//! 正在准备挂载的日志目录登记。
//!
//! 日志目录建好之后，到 Docker 应答之前（中间可能隔着一次很慢的镜像拉取，多久没有上界），
//! 它在挂载清单里还看不到。不能靠"目录还年轻"来保护，改为按路径登记：登记期间清理器把它
//! 当作仍被挂载。登记跟着 [`PendingBinds`] 走，Docker 应答之后才注销。
//!
//! # 清理器的读取顺序
//!
//! 清理器必须**先**读登记、**后**取 Docker 清单。登记只在 Docker 应答之后注销，所以对任何
//! 一个目录，二者必居其一：这次读到了它的登记；或者登记已经注销——那说明 Docker 已经应答，
//! 容器要么在（之后的）清单里，要么确实没建成。反过来先取清单再读登记会两头落空：取清单时
//! 容器还没登记，读登记时登记已经注销。
//!
//! 每个容器只登记自己的目录，互不影响：一次卡住的删除不会挡住别的容器启动。
//!
//! 登记随守卫 drop 注销，Docker 的应答丢了也一样。那之后到 Docker 处理完这次创建之间，目录
//! 还在保留期下限之内、成不了候选；等它成了候选，Docker 早就处理完了：建出来了就在清单里，
//! 没建出来就该回收。登记本身的过期时间只是创建流程卡死时的兜底。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use moka::sync::Cache;

/// 一条登记最长保留多久。正常情况下随守卫 drop 注销；这是创建流程卡死、守卫一直不 drop
/// 时的兜底
const PENDING_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// 最多登记多少条，防止异常情况下无限增长
const PENDING_MAX_ENTRIES: u64 = 10_000;

static PENDING: std::sync::OnceLock<Cache<PathBuf, Instant>> = std::sync::OnceLock::new();

fn pending() -> &'static Cache<PathBuf, Instant> {
    PENDING.get_or_init(|| {
        Cache::builder()
            .time_to_live(PENDING_TTL)
            .max_capacity(PENDING_MAX_ENTRIES)
            .build()
    })
}

/// 正在准备挂载的日志目录（宿主机路径）；drop 时全部注销。只能在 `log_directory` 里登记
#[derive(Debug, Default)]
pub struct PendingBinds(Vec<PathBuf>);

impl PendingBinds {
    /// 登记一个宿主机路径，直到本守卫 drop
    pub(crate) fn claim(&mut self, host_path: &Path) {
        let path = crate::normalize_path(host_path);
        pending().insert(path.clone(), Instant::now());
        self.0.push(path);
    }
}

impl Drop for PendingBinds {
    fn drop(&mut self) {
        for path in &self.0 {
            pending().invalidate(path);
        }
    }
}

/// 当前登记着的宿主机路径
pub(crate) fn pending_bind_sources() -> Vec<PathBuf> {
    pending()
        .iter()
        .map(|(path, _)| PathBuf::from(path.as_ref()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claim_is_visible_until_its_guard_is_dropped() {
        assert!(pending_bind_sources().is_empty());

        let mut binds = PendingBinds::default();
        binds.claim(Path::new("/srv/logs/container/runner-a"));
        binds.claim(Path::new("/srv/logs/container/runner-b"));
        let mut visible = pending_bind_sources();
        visible.sort();
        assert_eq!(
            visible,
            [
                PathBuf::from("/srv/logs/container/runner-a"),
                PathBuf::from("/srv/logs/container/runner-b")
            ]
        );

        drop(binds);
        assert!(pending_bind_sources().is_empty());
    }

    #[test]
    fn a_claim_is_keyed_by_its_normalized_path() {
        let mut binds = PendingBinds::default();
        binds.claim(Path::new("/srv/logs/container/./runner-a/"));

        assert_eq!(
            pending_bind_sources(),
            [PathBuf::from("/srv/logs/container/runner-a")]
        );
    }

    /// 注销只动自己登记过的路径
    #[test]
    fn dropping_one_guard_leaves_the_others_claimed() {
        let mut first = PendingBinds::default();
        first.claim(Path::new("/srv/logs/container/runner-a"));
        let mut second = PendingBinds::default();
        second.claim(Path::new("/srv/logs/container/runner-b"));

        drop(first);

        assert_eq!(
            pending_bind_sources(),
            [PathBuf::from("/srv/logs/container/runner-b")]
        );
    }
}
