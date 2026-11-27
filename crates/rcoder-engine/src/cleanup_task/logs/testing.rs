//! 日志清理的测试夹具：假的挂载判定和一个"早就过期"的目录。日志清理器自己的测试和日志
//! 清理任务的调度测试共用。

use async_trait::async_trait;
use container_runtime_api::{LogDirectoryMounts, ReclaimPermit, ReclaimRefused};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::cleaner::LogCleaner;
use super::owner::MountedPathsProbe;

/// 假的挂载判定：`mounted` 里的路径回答"仍被容器挂着"，`unconfirmed` 里的回答"无法确认"，
/// 其余路径回答"没有容器挂"；`failing` 为 true 时整份清单拿不到。同时记录被询问的次数。
pub(in crate::cleanup_task) struct FakeInventory {
    /// 被认为"仍被挂载"的宿主机路径；测试中途可以改动，用来模拟容器被回收
    pub(in crate::cleanup_task) mounted: Mutex<HashSet<PathBuf>>,
    /// 被判为"无法确认"的路径
    pub(in crate::cleanup_task) unconfirmed: Mutex<Vec<PathBuf>>,
    /// 被询问时顺手删掉的目录，用来模拟"列目录之后、删除之前目录消失"
    pub(in crate::cleanup_task) vanish_on_probe: Mutex<Vec<PathBuf>>,
    /// 被询问时顺手建出来的目录，用来模拟"扫描之后有容器刚启动"
    pub(in crate::cleanup_task) create_on_probe: Mutex<Vec<PathBuf>>,
    /// 最后一次确认时拒绝删除的目录：模拟"创建方在清理器取完清单之后才登记了它"
    pub(in crate::cleanup_task) refused_reclaims: Mutex<Vec<PathBuf>>,
    /// 最后一次确认问过的目录，按询问顺序
    pub(in crate::cleanup_task) reclaim_checks: Mutex<Vec<PathBuf>>,
    /// 最后一次确认拒绝时给出的原因种类。默认"仍被占用"
    pub(in crate::cleanup_task) reclaim_unconfirmed: Mutex<bool>,
    pub(in crate::cleanup_task) failing: Mutex<bool>,
    /// 回答之前先等这么久，用来模拟很慢的 Docker
    pub(in crate::cleanup_task) delay: Mutex<Duration>,
    pub(in crate::cleanup_task) calls: AtomicUsize,
}

impl FakeInventory {
    pub(in crate::cleanup_task) fn new(mounted: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            mounted: Mutex::new(mounted.iter().map(PathBuf::from).collect()),
            unconfirmed: Mutex::new(Vec::new()),
            vanish_on_probe: Mutex::new(Vec::new()),
            create_on_probe: Mutex::new(Vec::new()),
            refused_reclaims: Mutex::new(Vec::new()),
            reclaim_checks: Mutex::new(Vec::new()),
            reclaim_unconfirmed: Mutex::new(false),
            failing: Mutex::new(false),
            delay: Mutex::new(Duration::ZERO),
            calls: AtomicUsize::new(0),
        })
    }

    pub(in crate::cleanup_task) fn set_mounted(&self, path: &str, mounted: bool) {
        let mut set = self.mounted.lock().unwrap();
        if mounted {
            set.insert(PathBuf::from(path));
        } else {
            set.remove(PathBuf::from(path).as_path());
        }
    }

    pub(in crate::cleanup_task) fn set_unconfirmed(&self, path: &Path) {
        self.unconfirmed.lock().unwrap().push(path.to_path_buf());
    }

    pub(in crate::cleanup_task) fn vanish_on_probe(&self, path: &Path) {
        self.vanish_on_probe
            .lock()
            .unwrap()
            .push(path.to_path_buf());
    }

    pub(in crate::cleanup_task) fn create_on_probe(&self, path: &Path) {
        self.create_on_probe
            .lock()
            .unwrap()
            .push(path.to_path_buf());
    }

    /// 最后一次确认时拒绝删除 `path`
    pub(in crate::cleanup_task) fn refuse_reclaim(&self, path: &Path) {
        self.refused_reclaims
            .lock()
            .unwrap()
            .push(path.to_path_buf());
    }

    /// 拒绝时说"判不出来"，而不是"仍被占用"
    pub(in crate::cleanup_task) fn refuse_reclaim_as_unconfirmed(&self, path: &Path) {
        self.refuse_reclaim(path);
        *self.reclaim_unconfirmed.lock().unwrap() = true;
    }

    /// 最后一次确认问过的目录，按询问顺序
    pub(in crate::cleanup_task) fn reclaim_checks(&self) -> Vec<PathBuf> {
        self.reclaim_checks.lock().unwrap().clone()
    }

    pub(in crate::cleanup_task) fn set_failing(&self, failing: bool) {
        *self.failing.lock().unwrap() = failing;
    }

    pub(in crate::cleanup_task) fn set_delay(&self, delay: Duration) {
        *self.delay.lock().unwrap() = delay;
    }

    pub(in crate::cleanup_task) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl MountedPathsProbe for FakeInventory {
    async fn mounted_log_directories(
        &self,
        _candidates: &[PathBuf],
    ) -> Result<LogDirectoryMounts, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if *self.failing.lock().unwrap() {
            return Err("docker unreachable".to_string());
        }
        let delay = *self.delay.lock().unwrap();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        for path in self.vanish_on_probe.lock().unwrap().drain(..) {
            std::fs::remove_dir_all(path).expect("remove the vanished directory");
        }
        for path in self.create_on_probe.lock().unwrap().drain(..) {
            std::fs::create_dir_all(&path).expect("create the directory of a starting container");
        }
        Ok(LogDirectoryMounts {
            mounted: self.mounted.lock().unwrap().clone(),
            unconfirmed: self
                .unconfirmed
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .map(|path| (path, "fixture cannot confirm it".to_owned()))
                .collect(),
        })
    }

    async fn begin_reclaim(&self, candidate: &Path) -> Result<ReclaimPermit, ReclaimRefused> {
        self.reclaim_checks.lock().unwrap().push(candidate.to_path_buf());
        if self
            .refused_reclaims
            .lock()
            .unwrap()
            .contains(&candidate.to_path_buf())
        {
            let reason = "fixture: a starting container registered it after the inventory was read";
            return Err(if *self.reclaim_unconfirmed.lock().unwrap() {
                ReclaimRefused::Unconfirmed(reason.to_owned())
            } else {
                ReclaimRefused::InUse(reason.to_owned())
            });
        }
        Ok(ReclaimPermit::noop())
    }
}

/// 建一个内容为空的目录并把它标记为 `days` 天前修改
pub(in crate::cleanup_task) fn aged_dir(parent: &Path, name: &str, days: u64) -> PathBuf {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir).expect("create dir");
    let old_time = std::time::SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
    std::fs::set_times(&dir, std::fs::FileTimes::new().set_modified(old_time))
        .expect("set directory mtime");
    dir
}

pub(in crate::cleanup_task) fn cleaner(
    log_dir: &Path,
    inventory: Arc<FakeInventory>,
) -> LogCleaner {
    LogCleaner::new(log_dir.to_str().expect("utf-8 path"), 10, inventory)
}
