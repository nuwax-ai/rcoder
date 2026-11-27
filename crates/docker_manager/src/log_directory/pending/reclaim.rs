//! 清理器删除和创建方登记之间的目录级互斥。
//!
//! 清理器一轮只读一次登记。创建方在它读完之后才登记的，清理器不知道，可能正好在删创建方要
//! 复用的那个旧目录。所以删除之前清理器要回来做最后一次确认：先给目录打上"正在删"的标记
//! （[`mark_reclaim`]），再核对登记。标记之后才登记的创建方一定看得到标记、会等这次删除
//! （[`wait_until_released`]），所以核对时登记里没有就是真没有。创建方的登记
//! （[`add_claim`]）和清理器的标记用的是同一张表、同一把锁：
//!
//! - 创建方先登记：清理器的核对看得到，放弃这次删除；
//! - 清理器先打标记：创建方登记后等清理器删完再往下走。
//!
//! 核对（"有没有创建方登记着它或它下面的路径"）不在这里做：它用的判定要和清理器取清单时
//! 一致，在 `runtime::docker_runtime::log_dir_mounts`。这里只有标记和等待。
//!
//! 锁里只有几次内存操作，谁也不在持锁时做 I/O。会等的只有"要绑的目录恰好正在被删"的创建
//! 方，等的是这一次删除，最多 [`RECLAIM_WAIT_LIMIT`]——不是整轮清理，更不是别的容器。新建的
//! 日志目录是独占新建的，从来不会撞上删除；会等的只有复用旧目录的创建。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

/// 创建方登记一个正被删除的目录时，最多等这次删除这么久
const RECLAIM_WAIT_LIMIT: Duration = Duration::from_secs(10);

/// 测试用：把上面这个时限改成这么多毫秒
#[cfg(test)]
static RECLAIM_WAIT_LIMIT_MILLIS: AtomicU64 = AtomicU64::new(0);

/// 等待期间隔多久看一次删完没有。叫醒只对已经在等的生效，所以留一个兜底的轮询
const RECLAIM_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// 在途登记和"正在删"的标记，同一把锁保护
#[derive(Default)]
struct Registry {
    /// 宿主机路径 → 登记着它的守卫数
    claims: HashMap<PathBuf, usize>,
    /// 清理器正在删的宿主机路径
    deleting: HashSet<PathBuf>,
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

/// 一个目录删完了，叫醒等它的创建方
static RELEASED: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// 这张表。别的线程拿着锁时 panic 不影响表本身（里面只有路径和计数），照常使用
fn lock() -> MutexGuard<'static, Registry> {
    REGISTRY
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

fn wait_limit() -> Duration {
    #[cfg(not(test))]
    {
        RECLAIM_WAIT_LIMIT
    }
    #[cfg(test)]
    {
        match RECLAIM_WAIT_LIMIT_MILLIS.load(Ordering::SeqCst) {
            0 => RECLAIM_WAIT_LIMIT,
            millis => Duration::from_millis(millis),
        }
    }
}

/// `path` 就是一个正在被删的目录，或者在它里面
fn being_reclaimed(registry: &Registry, path: &Path) -> bool {
    registry
        .deleting
        .iter()
        .any(|deleting| path.strip_prefix(deleting).is_ok())
}

/// 登记一个宿主机路径：守卫计数加一。返回登记用的键（归一化之后的路径）
pub(super) fn add_claim(host_path: &Path) -> PathBuf {
    let path = crate::normalize_path(host_path);
    *lock().claims.entry(path.clone()).or_insert(0) += 1;
    path
}

/// 注销一次登记：守卫计数减一，最后一个守卫退出时条目消失。`host_path` 是登记时返回的键
pub(super) fn remove_claim(host_path: &Path) {
    let mut registry = lock();
    let Some(count) = registry.claims.get_mut(host_path) else {
        return;
    };
    if *count > 1 {
        *count -= 1;
    } else {
        registry.claims.remove(host_path);
    }
}

/// 在途登记着的宿主机路径
pub(super) fn claimed_sources() -> Vec<PathBuf> {
    lock().claims.keys().cloned().collect()
}

/// 创建方已经登记了 `host_path`（登记时返回的键）。它要是正被清理器删，等删完；超过时限
/// 还在删就报错。
///
/// 等到了也只是删完了：目录已经不在，后面的创建由 Docker 明确拒绝（源目录不存在），不会绑到
/// 一个正在被删的目录上
pub(super) async fn wait_until_released(host_path: &Path) -> Result<(), DirectoryBeingReclaimed> {
    if !being_reclaimed(&lock(), host_path) {
        return Ok(());
    }
    let deadline = tokio::time::Instant::now() + wait_limit();
    while being_reclaimed(&lock(), host_path) {
        if tokio::time::Instant::now() >= deadline {
            return Err(DirectoryBeingReclaimed(host_path.to_path_buf()));
        }
        let _ = tokio::time::timeout(RECLAIM_POLL_INTERVAL, RELEASED.notified()).await;
    }
    Ok(())
}

/// 给 `host_path` 打上"正在删"的标记，标记随返回值 drop 解除。它已经在删了（另一轮清理在删
/// 同一个目录）就拒绝：那一轮有自己的标记，删完就解除，这一轮不能再删它
pub(super) fn mark_reclaim(host_path: &Path) -> Result<ReclaimMark, String> {
    let path = crate::normalize_path(host_path);
    let mut registry = lock();
    if !registry.deleting.insert(path.clone()) {
        return Err(format!(
            "{} is already being reclaimed by another sweep",
            path.display()
        ));
    }
    Ok(ReclaimMark(path))
}

/// "正在删"的标记：drop 时解除，并叫醒等它的创建方
pub(crate) struct ReclaimMark(PathBuf);

impl Drop for ReclaimMark {
    fn drop(&mut self) {
        let removed = lock().deleting.remove(&self.0);
        if removed {
            RELEASED.notify_waiters();
        }
    }
}

/// 创建方要绑的目录正被清理器删除，等到了时限还没删完
#[derive(Debug, Eq, PartialEq)]
pub struct DirectoryBeingReclaimed(PathBuf);

impl std::fmt::Display for DirectoryBeingReclaimed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the directory {} is still being reclaimed by the log cleaner; try again later",
            self.0.display()
        )
    }
}

impl std::error::Error for DirectoryBeingReclaimed {}

#[cfg(test)]
mod tests;
