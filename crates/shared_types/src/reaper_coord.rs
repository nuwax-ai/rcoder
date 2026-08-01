//! Reaper 协调注册表: 记录 `tokio::process` 拥有的子进程 PID。
//!
//! 背景: agent_runner 作 PID 1 时, [`process_reaper`] 负责回收孤儿进程。
//! 但旧实现用 `waitpid(-1)` 无差别回收, 会抢走 tokio 正在 `child.wait()` 的子进程 →
//! tokio 拿到 ECHILD ("No child processes")。`tokio::process` 与同一进程内的 `waitpid(-1)`
//! 是已知冲突反模式。
//!
//! 解法: 所有 `tokio::process::Command::spawn` 点在 spawn 后立即 [`register_tokio_pid`];
//! reaper 回收时经 [`is_tokio_owned`] 跳过这些 PID (留给 tokio 自己 wait), 只回收真·孤儿。
//! 注册表由 reaper 定期 [`prune_dead_pids`] 自清理 (进程已退出 + 被 tokio 回收的 PID)。
//!
//! 关键无 race 保证: `spawn()` 返回时子进程刚 exec 完是 **活的**, 不是僵尸;
//! 它要之后才死, 那时早已登记。故 spawn→登记之间没有"已死"窗口 (exec 即退的极端边缘除外)。

use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::OnceLock;

static TOKIO_OWNED_PIDS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();

fn pids() -> &'static Mutex<HashSet<u32>> {
    TOKIO_OWNED_PIDS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// spawn 后立即登记: 标记该 PID 归 tokio::process 拥有, reaper 不得回收。
///
/// 必须在 `Command::spawn()` 成功返回后、子进程可能退出前调用。
/// spawn 返回时子进程是活的 → 这里登记无 race。
pub fn register_tokio_pid(pid: u32) {
    pids().lock().insert(pid);
}

/// reaper 查询: 该 PID 是否归 tokio 拥有。是则跳过 (留给 tokio `child.wait()`), 不抢。
pub fn is_tokio_owned(pid: u32) -> bool {
    pids().lock().contains(&pid)
}

/// reaper 定期自清理: 仅保留当前仍在 /proc 中存活 (live_pids) 的 PID。
///
/// 进程退出 + 被 tokio 回收后, PID 从 /proc 消失 → 从注册表移除。
/// 避免 PID 复用: 否则一个已退出的 tokio 子进程 PID 被复用成新孤儿时,
/// reaper 会误判为 tokio 拥有而漏回收。
pub fn prune_dead_pids(live_pids: &HashSet<u32>) {
    let mut guard = pids().lock();
    guard.retain(|pid| live_pids.contains(pid));
}
