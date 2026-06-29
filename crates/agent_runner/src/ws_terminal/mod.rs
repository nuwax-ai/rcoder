//! ttyd WebSocket 终端中间层
//!
//! agent_runner 用 tokio-tungstenite 在浏览器和本地 ttyd 之间做 WS 中间控制层：
//! - 对外：监听 7681，接浏览器 WS（子协议 `tty`，实现 ttyd 二进制帧透传）
//! - 对内：用 `connect_async` 连本地 ttyd（`ws://127.0.0.1:17681/ws`），
//!   ttyd 退到内部端口（17681），仍提供真实 PTY/终端
//!
//! cd 逻辑由本模块代码每次连接（含重连）控制：从 Pingora 注入的
//! `X-Ttyd-Project-Id` header 拿 project_id，连接 ttyd 时注入 `arg=--cwd&arg={项目目录}`，
//! 彻底摆脱「Pingora `upstream_request_filter` 对 WS 只首次触发」的结构性缺陷。
//!
//! 后续可在中间层增量扩展：WS 鉴权、session 持久化、命令审计等。

use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};

pub mod cwd;
pub mod protocol;
pub mod proxy;
pub mod server;

pub use server::start_ws_terminal;

/// 当前容器内活跃的终端 WS 连接数（每个 agent_runner 进程 = 一个容器，
/// 故进程级全局计数恰好等于该容器的终端连接数）。
///
/// 供 `GetContainerStatus` 读取，使 idle cleaner 的 gRPC 二次确认在「终端在用」时
/// 返回 `is_active=true`，避免容器被空闲清理误杀（终端流量本身不刷新 last_activity、
/// 也不计入 agent active task，见 `grpc::status::get_active_tasks_count`）。
pub static ACTIVE_TERMINAL_CONNS: LazyLock<AtomicUsize> = LazyLock::new(AtomicUsize::default);

/// 当前容器的活跃终端连接数。
pub fn active_terminal_count() -> usize {
    ACTIVE_TERMINAL_CONNS.load(Ordering::Relaxed)
}

/// 终端连接计数 RAII guard：构造时 +1，Drop 时 -1。
///
/// 在 `server::handle_conn` 中包住 `proxy::handle_terminal` 调用，覆盖其全部 return 路径
/// （proxy.rs 三处正常 return + `relay` 的 `tokio::join!` 结束），保证计数不泄漏。
pub(crate) struct TerminalConnGuard;

impl TerminalConnGuard {
    pub(crate) fn new() -> Self {
        ACTIVE_TERMINAL_CONNS.fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for TerminalConnGuard {
    fn drop(&mut self) {
        ACTIVE_TERMINAL_CONNS.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 计数器是进程级全局，cargo test 默认并发执行会互相干扰，
    /// 这里用一把互斥锁把这些测试串行化，确保各自的基线/断言确定。
    /// （测试二进制内没有真实 ws_terminal 服务，基线恒为 0。）
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn guard_increments_and_decrements_on_drop() {
        let _l = lock();
        let baseline = active_terminal_count();
        {
            let _g = TerminalConnGuard::new();
            assert_eq!(active_terminal_count(), baseline + 1);
        }
        assert_eq!(active_terminal_count(), baseline);
    }

    #[test]
    fn multiple_guards_accumulate_and_release() {
        let _l = lock();
        let baseline = active_terminal_count();
        let mut guards: Vec<TerminalConnGuard> = (0..5).map(|_| TerminalConnGuard::new()).collect();
        assert_eq!(active_terminal_count(), baseline + 5);
        guards.clear();
        assert_eq!(active_terminal_count(), baseline);
    }

    #[test]
    fn parallel_guards_do_not_leak() {
        let _l = lock();
        let baseline = active_terminal_count();
        let n = 64;
        std::thread::scope(|s| {
            for _ in 0..n {
                s.spawn(|| {
                    let _g = TerminalConnGuard::new();
                    assert!(active_terminal_count() > baseline);
                });
            }
        });
        // 所有线程退出后，计数必须回落到基线（无泄漏）。
        assert_eq!(active_terminal_count(), baseline);
    }
}
