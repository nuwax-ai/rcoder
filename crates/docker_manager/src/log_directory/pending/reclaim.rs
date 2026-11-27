//! 清理器删除和创建方登记之间的目录级互斥：一张表、一把锁。
//!
//! 表里有三样东西：
//!
//! - **在途登记**：创建方准备绑定的宿主机路径，按守卫计数；
//! - **"正在删"的标记**：清理器正在删的目录，一次删除一个标记，删完才解除；
//! - **代次**：每次登记加上、或者最后一个守卫注销，都加一。
//!
//! 清理器一轮只取一次清单，之后才开始的创建它看不到。所以删每个目录之前要回来做最后一次
//! 确认，在同一次持锁里：先打标记，再读登记和代次。
//!
//! - 创建方先登记：登记在表里，清理器看得到，放弃这次删除；
//! - 清理器先打标记：创建方登记后看得到标记，等这次删除结束。
//!
//! 代次管的是第三种交错：创建方登记、Docker 把容器建出来、创建方注销，全都发生在清理器取
//! 清单**之后**。那时登记已经不在表里，容器只在 Docker 里，只读登记表看不到它。清理器取清单
//! 时记下代次，打标记时再读一次：不一样就说明清单之后有创建开始或者结束，那份清单已经不代表
//! 现在，要在标记之下重新取一次 Docker 清单
//! （`runtime::docker_runtime::log_dir_mounts::begin_reclaim`）。
//!
//! 这里只有表。两边的判定都不在这里，用的是同一个函数：清理器核对"有没有登记落在被删目录
//! 里"，创建方判断"要绑的目录是不是正被删"，都按目录身份比，见 `log_dir_mounts`。
//!
//! 锁里只有几次内存操作，谁也不在持锁时做 I/O。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

/// 这张表某一刻的样子：在途登记和代次，同一次持锁读出
#[derive(Debug, Default)]
pub(crate) struct RegistryView {
    /// 在途登记着的宿主机路径
    pub(crate) sources: Vec<PathBuf>,
    /// 代次
    pub(crate) generation: u64,
}

#[derive(Default)]
struct Registry {
    /// 宿主机路径 → 登记着它的守卫数
    claims: HashMap<PathBuf, usize>,
    /// 清理器正在删的目录：宿主机路径 → 同一个目录在 RCoder 进程里的路径
    deleting: HashMap<PathBuf, PathBuf>,
    /// 每次登记加上、最后一个守卫注销，都加一
    generation: u64,
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

fn view_of(registry: &Registry) -> RegistryView {
    RegistryView {
        sources: registry.claims.keys().cloned().collect(),
        generation: registry.generation,
    }
}

/// 登记一个宿主机路径：守卫计数加一。返回登记用的键（归一化之后的路径）
pub(super) fn add_claim(host_path: &Path) -> PathBuf {
    let path = crate::normalize_path(host_path);
    let mut registry = lock();
    *registry.claims.entry(path.clone()).or_insert(0) += 1;
    registry.generation += 1;
    path
}

/// 注销一次登记：守卫计数减一，最后一个守卫退出时条目消失。`key` 是登记时返回的键
pub(super) fn remove_claim(key: &Path) {
    let mut registry = lock();
    let Some(count) = registry.claims.get_mut(key) else {
        return;
    };
    if *count > 1 {
        *count -= 1;
    } else {
        registry.claims.remove(key);
        registry.generation += 1;
    }
}

/// 在途登记和代次，同一次持锁读出
pub(super) fn view() -> RegistryView {
    view_of(&lock())
}

/// 等一个目录删完：下一次有标记解除时醒来
pub(super) fn released() -> impl Future<Output = ()> + Send + 'static {
    RELEASED.notified()
}

/// 清理器此刻正在删的目录，每项是（宿主机路径，同一个目录在 RCoder 进程里的路径）
pub(super) fn marks() -> Vec<(PathBuf, PathBuf)> {
    lock()
        .deleting
        .iter()
        .map(|(host, process)| (host.clone(), process.clone()))
        .collect()
}

/// 给一个目录打上"正在删"的标记，标记随返回值 drop 解除。和读登记、读代次是同一次持锁：
/// 返回的那份视图就是打上标记那一刻的表，之后才登记的创建方一定看得到标记。
///
/// 它已经在删了（另一次删除在删同一个目录）就拒绝：那一次有自己的标记，删完才解除
pub(super) fn mark_reclaim(
    host_path: &Path,
    process_path: &Path,
) -> Result<(ReclaimMark, RegistryView), String> {
    let host = crate::normalize_path(host_path);
    let mut registry = lock();
    if registry.deleting.contains_key(&host) {
        return Err(format!(
            "{} is already being reclaimed by another deletion",
            host.display()
        ));
    }
    registry
        .deleting
        .insert(host.clone(), process_path.to_path_buf());
    Ok((ReclaimMark(host), view_of(&registry)))
}

/// "正在删"的标记：drop 时解除，并叫醒等它的创建方
pub(crate) struct ReclaimMark(PathBuf);

impl Drop for ReclaimMark {
    fn drop(&mut self) {
        let removed = lock().deleting.remove(&self.0).is_some();
        if removed {
            RELEASED.notify_waiters();
        }
    }
}

/// 创建方要绑的目录正被清理器删除，等到了时限还没删完
#[derive(Debug, Eq, PartialEq)]
pub struct DirectoryBeingReclaimed(pub(crate) PathBuf);

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
