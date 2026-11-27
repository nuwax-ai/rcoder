//! 创建方登记一个宿主机路径时，判断它是不是正被清理器删除；是的话等这次删除结束。
//!
//! 判断和清理器那边的核对是同一个函数（[`super::classify`]），只是角色对调：被删的目录当
//! 候选，要登记的路径当挂载源。候选"仍被挂载"的意思是"它自己、或者它里面的东西，被这个源
//! 指着"——正是"要绑的目录落在被删目录里面"。所以两边对"同一个目录"的定义完全一致：按目录
//! 身份比，经别名登记的也认得出。
//!
//! 只比路径拼写是不够的：清理器拿着 `logs/runner-a` 的许可时，同一个目录可以从 `alias/runner-a`
//! 登记进来，拼写不同，删的却是它。
//!
//! 要登记的路径**还没建出来**时没有身份可比（创建方常常是先登记、稍后才在下面建目录）。这时看
//! 它会建在哪一级已存在的祖先下面：按真实位置，祖先等于被删目录或在它里面，就是在同一棵正在
//! 删的目录树里。
//!
//! 探测（读目录身份）和等待共用同一个截止时间：探测自己卡住（文件系统不响应）也算等不到。

use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

use super::{LocalFileSystem, Location, LogDirMount, RealLocations, classify, observe};
use crate::log_directory::{DirectoryBeingReclaimed, reclaim_marks, reclaim_released};
use crate::runtime::docker_runtime::host_path_space::HostPathSpace;

/// 创建方登记一个正被删除的目录时，最多等这次删除这么久——探测和等待加在一起
const RECLAIM_WAIT_LIMIT: Duration = Duration::from_secs(10);

/// 测试用：把上面这个时限改成这么多毫秒
#[cfg(test)]
static RECLAIM_WAIT_LIMIT_MILLIS: AtomicU64 = AtomicU64::new(0);

/// 等待期间隔多久看一次删完没有。叫醒只对已经在等的生效，所以留一个兜底的轮询
const RECLAIM_POLL_INTERVAL: Duration = Duration::from_millis(50);

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

/// `claim`（宿主机路径）已经登记进表。它落在某个正在删的目录里面的话，等那次删除结束；等不过
/// 时限就报错。
///
/// 时限管的是整个调用：探测目录身份也可能卡住，所以探测和等待共用同一个截止时间。探测没在
/// 时限内返回，说明不了它和正在删的目录不相干，同样按等不到处理。
///
/// 等到了也只是删完了：目录已经不在，后面的创建由 Docker 明确拒绝（源目录不存在），不会绑到
/// 一个正在被删的目录上。
///
/// 先登记、后读标记，顺序不能反：读完标记之后才打上的标记，打它的那次删除会读到这里已经登记
/// 的 `claim`，自己放弃。
///
/// `space` 是这次创建用的路径空间。没有它核不了目录身份：有目录正在删就宁可等它删完
pub(crate) async fn wait_while_reclaiming(
    claim: &Path,
    space: Option<&HostPathSpace<'_>>,
) -> Result<(), DirectoryBeingReclaimed> {
    let deadline = tokio::time::Instant::now() + wait_limit();
    // 已经确认过一次的冲突。之后探测被截止时间打断时，报的是它
    let mut confirmed: Option<PathBuf> = None;
    loop {
        let holding = tokio::time::timeout_at(deadline, first_reclaim_holding(claim, space)).await;
        let reclaiming = match holding {
            Ok(None) => return Ok(()),
            Ok(Some(reclaiming)) => {
                confirmed = Some(reclaiming.clone());
                reclaiming
            }
            Err(_elapsed) => {
                return Err(match confirmed {
                    // 冲突是确认过的：等的就是那次删除，等不到
                    Some(reclaiming) => DirectoryBeingReclaimed::conflict(reclaiming),
                    // 还什么都没确认：说明不了它和正在删的目录不相干
                    None => DirectoryBeingReclaimed::unconfirmed(some_mark(claim)),
                });
            }
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(DirectoryBeingReclaimed::conflict(reclaiming));
        }
        let _ = tokio::time::timeout(RECLAIM_POLL_INTERVAL, reclaim_released()).await;
    }
}

/// 正在删的目录里，第一个装得下 `claim` 的（宿主机路径）；没有则 `None`
async fn first_reclaim_holding(claim: &Path, space: Option<&HostPathSpace<'_>>) -> Option<PathBuf> {
    for (host, process) in reclaim_marks() {
        let holds = match space {
            None => true,
            Some(space) => holds_bind(space, &host, &process, claim).await != Some(false),
        };
        if holds {
            return Some(host);
        }
    }
    None
}

/// `directory`（RCoder 进程里的路径，宿主机写法是 `host`）自己就是 `bind`（宿主机路径）指向的
/// 目录，或者装得下它——包括 `bind` 还没建出来的情形。`None` 是判不出来
async fn holds_bind(
    space: &HostPathSpace<'_>,
    host: &Path,
    directory: &Path,
    bind: &Path,
) -> Option<bool> {
    let fs = LocalFileSystem;
    let observations = observe(space, std::slice::from_ref(&bind.to_path_buf()), &fs).await;
    match classify(directory, space, &observations, &fs).await {
        LogDirMount::Mounted => return Some(true),
        LogDirMount::Unconfirmed(_) => return None,
        LogDirMount::Unmounted => {}
    }
    // 到这里 `bind` 和被删目录不相干——除非它还不存在，没有身份可比。创建方常常是先登记、稍后
    // 才在下面建目录，那时看它会建在哪一级已存在的祖先下面
    let mut locations = RealLocations::new(space, &fs);
    let absent = match locations.locate(bind).await {
        Ok(Location::Absent { path }) => path,
        // 已经在了，或者中间有一级是文件、或者整条路径在 RCoder 的挂载之外：上面的判定说了不算，就是不算
        Ok(_) => return Some(false),
        Err(_) => return None,
    };
    let marked = crate::normalize_path(&locations.real_location(host).await.ok()?);
    for ancestor in absent.ancestors().skip(1) {
        match locations.locate(ancestor).await {
            // 最近一级已存在的目录：`bind` 将来就建在它下面
            Ok(Location::Present {
                path: real,
                directory: true,
            }) => {
                return Some(crate::normalize_path(&real).starts_with(&marked));
            }
            Ok(Location::Absent { .. }) => continue,
            // 中间有一级是文件：这下面建不出目录来
            Ok(Location::Present {
                directory: false, ..
            }) => return Some(false),
            // 整条路径都在 RCoder 的挂载之外：被删目录在挂载之内，装不下它
            Ok(Location::Outside) => return Some(false),
            Ok(Location::Unconfirmed(_)) | Err(_) => return None,
        }
    }
    // 走到根为止一定有一级存在，到不了这里；真到了就是判不出来
    None
}

/// 探测超时的时候报错点名哪个目录：不知道卡在哪一个标记上，取第一个；一个标记都没有就报
/// 要登记的路径本身
fn some_mark(claim: &Path) -> PathBuf {
    reclaim_marks()
        .into_iter()
        .next()
        .map(|(host, _)| host)
        .unwrap_or_else(|| crate::normalize_path(claim))
}

#[cfg(test)]
pub(super) fn set_wait_limit_for_test(millis: u64) {
    RECLAIM_WAIT_LIMIT_MILLIS.store(millis, Ordering::SeqCst);
}
