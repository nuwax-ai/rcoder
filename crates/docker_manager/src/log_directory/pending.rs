//! 正在准备挂载的日志目录登记。
//!
//! 日志目录建好之后，到 Docker 确认结果之前（中间可能隔着一次很慢的镜像拉取，多久没有上界），
//! 它在挂载清单里看不到。不能靠"目录还年轻"来保护，改为按路径登记：登记期间清理器把它当作
//! 仍被挂载。创建容器和 builder 重启都用它。
//!
//! 判不出在不在日志根里的 bind 源也登记在这里：清理器定不了它的位置，于是保留所有候选。
//!
//! # 两类登记
//!
//! - **在途**：跟着守卫 [`PendingBinds`] 走，守卫 drop 时注销。在建、在等 Docker 的创建每次
//!   只登记自己用到的目录，条目数不会超过同时在建的创建数，所以不设容量、不淘汰：登记被淘汰
//!   等于保护失效。同一条路径可以同时被几个守卫登记（判不出来的 bind 源会被几个容器共用），
//!   按守卫计数，一个退出不影响其余的。
//! - **结果不明**：创建请求发出去了，却没能确认结果——应答丢了、daemon 报 5xx、连接被取消。
//!   这时容器可能已经建出来、正绑着这些目录，登记必须留着。它转进另一张带过期时间的表
//!   （[`UNRESOLVED_TTL`]），到期自动消失。不主动去对账：看不到容器不能证明请求没生效，
//!   多留一段时间的代价只是没人用的目录晚一点回收；过期时 Docker 早就处理完了这次创建，
//!   建出来了就在清单里，没建出来就该回收。
//!
//! # 清理器的读取顺序
//!
//! 清理器必须**先**读登记、**后**取 Docker 清单。在途登记只在 Docker 应答之后注销，所以对任何
//! 一个目录，三者必居其一：这次读到了它的登记；登记已经注销——那说明 Docker 已经应答，容器
//! 要么在（之后的）清单里，要么确实没建成；或者结果不明、登记被保留。
//!
//! 每个容器只登记自己用到的目录，互不影响：一次卡住的删除不会挡住别的容器启动。
//!
//! # 边界：登记要清理器读到才算
//!
//! 登记是按路径的，清理器一轮只读一次。创建方在清理器读完登记**之后**才登记的，这一轮看不到；
//! 它绑的如果是一个已经过期的旧目录（复用），清理器可能正好在删它。所以能换新目录的都换：
//! 新建的目录年轻，不是候选；落在日志根里的日志挂载，判得出判不出（只要归档能核实是日志
//! 挂载）都换。
//!
//! 真会复用过期目录的只剩两类：落在日志根里的持久数据（配置错误，创建容器时就拒绝，这里只是
//! 存量），和判不出来、又无法从归档核实角色的 bind。这个窗口是一轮回收的长度（预算 2 秒），
//! 只在这两类布局里存在。登记不淘汰、按守卫计数，保证的是"读得到的登记不会丢"。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

/// 结果不明的登记保留多久。这是兜底，不是创建流程的正常寿命
const UNRESOLVED_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// 在途登记：宿主机路径 → 登记着它的守卫数。只在登记、注销、读取时短暂持有，里面不做任何 I/O
static IN_FLIGHT: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

/// 在途登记的表。别的线程拿着锁时 panic 不影响表本身（里面只有路径和计数），照常使用
fn in_flight() -> MutexGuard<'static, HashMap<PathBuf, usize>> {
    IN_FLIGHT
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// 结果不明的登记，带过期时间
static UNRESOLVED: OnceLock<moka::sync::Cache<PathBuf, Instant>> = OnceLock::new();

/// 测试用：把结果不明登记的保留时长改成这么多毫秒。表是进程级的，要赶在它第一次用到之前改
#[cfg(test)]
static UNRESOLVED_TTL_MILLIS: AtomicU64 = AtomicU64::new(0);

fn unresolved() -> &'static moka::sync::Cache<PathBuf, Instant> {
    UNRESOLVED.get_or_init(|| {
        #[cfg(not(test))]
        let ttl = UNRESOLVED_TTL;
        #[cfg(test)]
        let ttl = match UNRESOLVED_TTL_MILLIS.load(Ordering::SeqCst) {
            0 => UNRESOLVED_TTL,
            millis => Duration::from_millis(millis),
        };
        moka::sync::Cache::builder().time_to_live(ttl).build()
    })
}

/// 正在准备挂载的日志目录（宿主机路径）。
///
/// 登记在 Docker 应答之前一直有效。调用方在发创建请求之前调 [`Self::request_sent`]，拿到确切
/// 结果之后：
/// - 容器建出来了，或者确认没建成：照常 drop，登记注销；
/// - 结果不明：调 [`Self::outcome_unknown`]，登记转去"结果不明"那张表，到期自动消失。
///
/// 请求发出之后调用方没来得及交代结果就 drop 了（比如被取消），按结果不明处理。
#[derive(Debug, Default)]
pub struct PendingBinds {
    paths: Vec<PathBuf>,
    request_sent: bool,
}

impl PendingBinds {
    /// 登记一个宿主机路径，直到这次创建有了结果。同一条路径可以同时被几个守卫登记：各登
    /// 各的，一个守卫退出不影响其余的
    pub(crate) fn claim(&mut self, host_path: &Path) {
        let path = crate::normalize_path(host_path);
        *in_flight().entry(path.clone()).or_insert(0) += 1;
        self.paths.push(path);
    }

    /// 创建请求马上发出
    pub(crate) fn request_sent(&mut self) {
        self.request_sent = true;
    }

    /// 创建结果已经确认——容器建出来了，或者确认没建成：守卫照常 drop，登记注销
    pub(crate) fn settled(&mut self) {
        self.request_sent = false;
    }

    /// 请求发出去了，结果不明：登记转去结果不明的那张表，保留到过期为止。守卫照常 drop，
    /// 它只注销在途登记
    pub(crate) fn outcome_unknown(&mut self) {
        for path in &self.paths {
            unresolved().insert(path.clone(), Instant::now());
        }
        self.request_sent = false;
    }

    /// 发出创建请求 `request`，登记跟着它的结果走：请求成功 → 注销；请求报错，或者等应答时
    /// 这个 future 被取消 → 结果不明，登记保留到过期（取消那条由 [`Drop`] 兜住）。
    ///
    /// 请求报错一律按结果不明处理：`create_container` 返回的错误里分不出"daemon 明确拒绝了"
    /// 和"应答丢了"。多留一段时间的代价只是没人用的目录晚一点回收，反过来则会撤掉一个可能
    /// 正被绑着的目录的保护。
    pub(crate) async fn until_settled<T, E>(
        &mut self,
        request: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        self.request_sent();
        match request.await {
            Ok(created) => {
                self.settled();
                Ok(created)
            }
            Err(error) => {
                self.outcome_unknown();
                Err(error)
            }
        }
    }
}

impl Drop for PendingBinds {
    fn drop(&mut self) {
        if self.request_sent {
            self.outcome_unknown();
        }
        if !self.paths.is_empty() {
            let mut in_flight = in_flight();
            for path in &self.paths {
                let Some(count) = in_flight.get_mut(path) else {
                    continue;
                };
                if *count > 1 {
                    *count -= 1;
                } else {
                    in_flight.remove(path);
                }
            }
        }
    }
}

/// 当前登记着的宿主机路径：在途的和结果不明的。同一条路径只列一次
pub(crate) fn pending_bind_sources() -> Vec<PathBuf> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    in_flight()
        .keys()
        .cloned()
        .chain(
            unresolved()
                .iter()
                .map(|(path, _)| PathBuf::from(path.as_ref())),
        )
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

#[cfg(test)]
mod tests;
