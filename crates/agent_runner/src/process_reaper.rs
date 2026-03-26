//! 僵尸进程回收器 (Zombie Process Reaper)
//!
//! 当 agent_runner 作为容器的 PID 1 运行时，它需要负责回收孤儿进程。
//! 此模块实现了一个基于 SIGCHLD 信号的子进程回收机制。
//!
//! # 设计原理
//!
//! 在 Linux 容器中，如果 PID 1 不调用 wait() 回收子进程，这些子进程
//! 退出后会变成僵尸进程（Zombie），占用系统资源。
//!
//! # 使用方式
//!
//! ```rust
//! use process_reaper::start_process_reaper;
//!
//! // 在主函数中启动回收器
//! let _reaper_handle = start_process_reaper();
//! ```

use std::collections::HashMap;
use std::fs;
use tokio::process::Child;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};
use tracing::{debug, error, info, warn};

/// 进程回收器配置
#[derive(Debug, Clone)]
pub struct ReaperConfig {
    /// 是否启用详细日志
    pub verbose: bool,
    /// 是否启用主动僵尸进程检测
    pub enable_zombie_detection: bool,
    /// 僵尸进程检测间隔（秒）
    pub zombie_detection_interval_secs: u64,
}

impl Default for ReaperConfig {
    fn default() -> Self {
        Self {
            verbose: false,
            enable_zombie_detection: true,
            zombie_detection_interval_secs: 10,
        }
    }
}

/// 僵尸进程信息
#[derive(Debug, Clone)]
pub struct ZombieProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub comm: String,
    pub state: char,
}

/// 进程回收器状态
#[derive(Debug)]
struct ReaperState {
    /// 追踪活跃的子进程
    /// 存储格式: pid -> Child
    active_children: HashMap<u32, Child>,
    /// 回收的进程总数
    reaped_count: u64,
    /// 检测到的僵尸进程数
    zombie_detected_count: u64,
    /// 配置
    config: ReaperConfig,
}

impl ReaperState {
    fn new(config: ReaperConfig) -> Self {
        Self {
            active_children: HashMap::new(),
            reaped_count: 0,
            zombie_detected_count: 0,
            config,
        }
    }

    /// 注册一个子进程，稍后自动回收
    fn register_child(&mut self, child: Child) {
        let id = child.id().unwrap_or(0);
        if id > 0 {
            self.active_children.insert(id, child);
            if self.config.verbose {
                debug!("[ProcessReaper] 注册子进程 PID={}", id);
            }
        }
    }

    /// 尝试回收所有已退出的子进程
    fn reap_all(&mut self) {
        let mut reaped_now = 0;

        // 使用 entry API 避免 DashMap/RwLock 问题（虽然这里是普通 HashMap）
        self.active_children.retain(|pid, child| {
            // 尝试查询进程状态（非阻塞）
            match child.try_wait() {
                Ok(Some(status)) => {
                    // 进程已退出
                    reaped_now += 1;
                    if self.config.verbose {
                        debug!(
                            "[ProcessReaper] 回收子进程 PID={}, exit_status={:?}",
                            pid, status
                        );
                    }
                    false // 移除已回收的进程
                }
                Ok(None) => {
                    // 进程仍在运行
                    true
                }
                Err(e) => {
                    // 查询失败，可能进程已不存在
                    warn!("[ProcessReaper] 查询子进程 PID={} 失败: {}", pid, e);
                    false // 移除无法查询的进程
                }
            }
        });

        if reaped_now > 0 {
            self.reaped_count += reaped_now;
            info!(
                "[ProcessReaper] 回收了 {} 个子进程 (总计: {})",
                reaped_now, self.reaped_count
            );
        }
    }

    /// 🔍 主动检测系统中的僵尸进程
    ///
    /// 扫描 /proc 文件系统，查找状态为 'Z' (Zombie) 的进程
    fn detect_zombie_processes(&mut self) -> Vec<ZombieProcessInfo> {
        let mut zombies = Vec::new();

        #[cfg(unix)]
        {
            let proc_path = "/proc";

            // 读取 /proc 目录下的所有 PID 目录
            if let Ok(entries) = fs::read_dir(proc_path) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    // 检查是否是数字（PID 目录）
                    if let Ok(pid) = name.to_string_lossy().parse::<u32>() {
                        // 读取 /proc/[pid]/stat 文件
                        let stat_path = entry.path().join("stat");
                        if let Ok(content) = fs::read_to_string(&stat_path)
                            && let Some(info) = parse_stat_file(pid, &content)
                            && info.state == 'Z'
                        {
                            zombies.push(info);
                        }
                    }
                }
            }
        }

        if !zombies.is_empty() {
            self.zombie_detected_count += zombies.len() as u64;
            warn!(
                "[ProcessReaper] 检测到 {} 个僵尸进程 (总计检测到: {})",
                zombies.len(),
                self.zombie_detected_count
            );

            for zombie in &zombies {
                warn!(
                    "[ProcessReaper] 僵尸进程: PID={}, PPID={}, CMD={}",
                    zombie.pid, zombie.ppid, zombie.comm
                );
            }
        }

        zombies
    }

    /// 🔧 主动清理所有僵尸进程
    ///
    /// 使用 waitpid 循环回收所有可能的僵尸进程，不仅仅是追踪的子进程
    /// 这是 PID 1 的责任：回收所有孤儿进程
    fn reap_all_zombies_blocking(&mut self) {
        #[cfg(unix)]
        {
            use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
            use nix::unistd::Pid;

            let mut reaped_this_round = 0;

            // 循环调用 waitpid，直到没有更多僵尸进程
            loop {
                match waitpid(
                    Pid::from_raw(-1), // -1 表示等待任意子进程
                    Some(WaitPidFlag::WNOHANG),
                ) {
                    Ok(WaitStatus::Exited(pid, exit_code)) => {
                        reaped_this_round += 1;
                        debug!(
                            "[ProcessReaper] 主动回收僵尸进程: PID={}, exit_code={}",
                            pid, exit_code
                        );
                    }
                    Ok(WaitStatus::Signaled(pid, signal, _)) => {
                        reaped_this_round += 1;
                        debug!(
                            "[ProcessReaper] 主动回收僵尸进程: PID={}, signal={:?}",
                            pid, signal
                        );
                    }
                    Ok(WaitStatus::StillAlive) => {
                        // WNOHANG: 没有更多的僵尸进程
                        break;
                    }
                    Ok(WaitStatus::Stopped(pid, signal)) => {
                        // 进程被停止（不是退出），不计入回收
                        debug!(
                            "[ProcessReaper] 进程被停止: PID={}, signal={:?}",
                            pid, signal
                        );
                        // 继续循环，可能还有其他僵尸进程
                        continue;
                    }
                    Ok(WaitStatus::Continued(pid)) => {
                        // 进程被恢复（SIGCONT），不计入回收
                        debug!("[ProcessReaper] 进程被恢复: PID={}", pid);
                        // 继续循环，可能还有其他僵尸进程
                        continue;
                    }
                    #[cfg(linux_android)]
                    Ok(WaitStatus::PtraceEvent(pid, signal, event)) => {
                        // ptrace 事件，不计入回收
                        debug!(
                            "[ProcessReaper] ptrace 事件: PID={}, signal={:?}, event={}",
                            pid, signal, event
                        );
                        continue;
                    }
                    #[cfg(linux_android)]
                    Ok(WaitStatus::PtraceSyscall(pid)) => {
                        // ptrace 系统调用，不计入回收
                        debug!("[ProcessReaper] ptrace 系统调用: PID={}", pid);
                        continue;
                    }
                    // 非Linux平台忽略 ptrace 相关状态（macOS 上 WaitStatus 包含这些变体但不会实际触发）
                    Ok(_) => {
                        debug!("[ProcessReaper] 忽略的 waitpid 状态");
                        continue;
                    }
                    Err(nix::errno::Errno::ECHILD) => {
                        // 没有子进程
                        break;
                    }
                    Err(e) => {
                        warn!("[ProcessReaper] waitpid 错误: {}", e);
                        break;
                    }
                }
            }

            if reaped_this_round > 0 {
                self.reaped_count += reaped_this_round;
                info!(
                    "[ProcessReaper] 主动回收了 {} 个僵尸进程 (总计: {})",
                    reaped_this_round, self.reaped_count
                );
            }
        }

        #[cfg(not(unix))]
        {
            debug!("[ProcessReaper] 非 Unix 平台，跳过僵尸进程检测");
        }
    }
}

/// 解析 /proc/[pid]/stat 文件
///
/// 文件格式：pid (comm) state ppid ...
/// 示例：1 (init) S 0 0 0 0 ...
fn parse_stat_file(pid: u32, content: &str) -> Option<ZombieProcessInfo> {
    // stat 文件格式：pid (comm) state ppid ...
    // 需要找到 comm 的结束括号
    let content = content.trim();

    // 找到第一个 '(' 和最后一个 ')'
    let open_paren = content.find('(')?;
    let close_paren = content.rfind(')')?;

    let comm = content[open_paren + 1..close_paren].to_string();
    let after_comm = &content[close_paren + 1..];

    // 解析 state 和 ppid
    // 格式：) state ppid ...
    let parts: Vec<&str> = after_comm.split_whitespace().collect();
    if parts.len() < 2 {
        return None;
    }

    let state = parts.first()?.chars().next()?;
    let ppid: u32 = parts.get(1)?.parse().ok()?;

    Some(ZombieProcessInfo {
        pid,
        ppid,
        comm,
        state,
    })
}

/// 启动进程回收器任务
///
/// 此函数会：
/// 1. 注册 SIGCHLD 信号处理器
/// 2. 在后台循环中等待信号并回收子进程
/// 3. 定期主动检测和清理僵尸进程
///
/// # 返回值
///
/// 返回一个 JoinHandle，可以用于等待回收器任务退出（通常不需要）
pub fn start_process_reaper() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { run_reaper(ReaperConfig::default()).await })
}

/// 启动进程回收器任务（带配置）
pub fn start_process_reaper_with_config(config: ReaperConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { run_reaper(config).await })
}

/// 核心回收逻辑
#[cfg(unix)]
async fn run_reaper(config: ReaperConfig) {
    info!("[ProcessReaper] 僵尸进程回收器已启动 (PID 1 模式)");
    if config.enable_zombie_detection {
        info!(
            "[ProcessReaper] 僵尸进程检测已启用，检测间隔: {} 秒",
            config.zombie_detection_interval_secs
        );
    }

    // 创建 SIGCHLD 信号监听器
    let sigchld = match signal(SignalKind::child()) {
        Ok(sig) => sig,
        Err(e) => {
            error!("[ProcessReaper] 无法注册 SIGCHLD 信号处理器: {}", e);
            error!("[ProcessReaper] 将使用轮询模式作为回退");

            // 回退模式：使用轮询
            run_reaper_polling(config).await;
            return;
        }
    };

    let state = ReaperState::new(config.clone());

    // 启动定期轮询任务（作为信号机制的补充）
    let mut poll_interval = tokio::time::interval(std::time::Duration::from_secs(5));
    poll_interval.tick().await; // 跳过第一次立即触发

    // 根据配置决定是否启用僵尸进程检测
    if config.enable_zombie_detection {
        run_reaper_with_detection(
            sigchld,
            poll_interval,
            state,
            config.zombie_detection_interval_secs,
        )
        .await
    } else {
        run_reaper_without_detection(sigchld, poll_interval, state).await
    }
}

/// Windows 上的回收逻辑（无操作，Windows 没有僵尸进程问题）
#[cfg(not(unix))]
async fn run_reaper(_config: ReaperConfig) {
    info!("[ProcessReaper] 非 Unix 平台，僵尸进程回收器不适用");
}

/// 🔍 启用僵尸进程检测的回收循环
#[cfg(unix)]
async fn run_reaper_with_detection(
    mut sigchld: tokio::signal::unix::Signal,
    mut poll_interval: tokio::time::Interval,
    mut state: ReaperState,
    detect_interval_secs: u64,
) {
    let mut zombie_detect_interval =
        tokio::time::interval(std::time::Duration::from_secs(detect_interval_secs));
    zombie_detect_interval.tick().await; // 跳过第一次立即触发

    loop {
        tokio::select! {
            // 等待 SIGCHLD 信号
            _ = sigchld.recv() => {
                if state.config.verbose {
                    debug!("[ProcessReaper] 收到 SIGCHLD 信号");
                }
                state.reap_all();
                state.reap_all_zombies_blocking();
            }
            // 定期轮询（每 5 秒）
            _ = poll_interval.tick() => {
                state.reap_all();
            }
            // 🔍 定期主动检测和清理僵尸进程
            _ = zombie_detect_interval.tick() => {
                debug!("[ProcessReaper] 开始定期僵尸进程检测...");

                // 先检测有哪些僵尸进程
                let zombies = state.detect_zombie_processes();

                // 然后主动清理所有僵尸进程
                if !zombies.is_empty() {
                    state.reap_all_zombies_blocking();
                }
            }
        }
    }
}

/// 🚫 不启用僵尸进程检测的回收循环
#[cfg(unix)]
async fn run_reaper_without_detection(
    mut sigchld: tokio::signal::unix::Signal,
    mut poll_interval: tokio::time::Interval,
    mut state: ReaperState,
) {
    loop {
        tokio::select! {
            // 等待 SIGCHLD 信号
            _ = sigchld.recv() => {
                if state.config.verbose {
                    debug!("[ProcessReaper] 收到 SIGCHLD 信号");
                }
                state.reap_all();
                state.reap_all_zombies_blocking();
            }
            // 定期轮询（每 5 秒）
            _ = poll_interval.tick() => {
                state.reap_all();
            }
        }
    }
}

/// 轮询模式回退（当信号机制不可用时）
#[cfg(unix)]
async fn run_reaper_polling(config: ReaperConfig) {
    info!("[ProcessReaper] 使用轮询模式回收僵尸进程");

    let state = ReaperState::new(config.clone());

    // 根据配置决定是否启用僵尸进程检测
    if config.enable_zombie_detection {
        run_reaper_polling_with_detection(state, config.zombie_detection_interval_secs).await
    } else {
        run_reaper_polling_without_detection(state).await
    }
}

/// 🔍 轮询模式 + 僵尸进程检测
#[cfg(unix)]
async fn run_reaper_polling_with_detection(mut state: ReaperState, detect_interval_secs: u64) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    let mut zombie_detect_interval =
        tokio::time::interval(std::time::Duration::from_secs(detect_interval_secs));
    zombie_detect_interval.tick().await;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                state.reap_all();
                state.reap_all_zombies_blocking();
            }
            _ = zombie_detect_interval.tick() => {
                let zombies = state.detect_zombie_processes();
                if !zombies.is_empty() {
                    state.reap_all_zombies_blocking();
                }
            }
        }
    }
}

/// 🚫 轮询模式（无僵尸进程检测）
#[cfg(unix)]
async fn run_reaper_polling_without_detection(mut state: ReaperState) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));

    loop {
        interval.tick().await;
        state.reap_all();
        state.reap_all_zombies_blocking();
    }
}

/// 回收器句柄（可选：用于外部注册子进程）
///
/// 注意：当前实现中，子进程由各自创建者管理。
/// 此结构保留用于未来扩展，例如中央化子进程管理。
#[derive(Debug, Clone)]
pub struct ProcessReaperHandle {
    _config: ReaperConfig,
}

impl ProcessReaperHandle {
    pub fn new() -> Self {
        Self {
            _config: ReaperConfig::default(),
        }
    }

    /// 🔍 手动触发僵尸进程检测（仅用于调试）
    pub fn detect_zombies_now(&self) -> Vec<ZombieProcessInfo> {
        #[cfg(unix)]
        {
            let proc_path = "/proc";
            let mut zombies = Vec::new();

            if let Ok(entries) = fs::read_dir(proc_path) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if let Ok(pid) = name.to_string_lossy().parse::<u32>() {
                        let stat_path = entry.path().join("stat");
                        if let Ok(content) = fs::read_to_string(&stat_path)
                            && let Some(info) = parse_stat_file(pid, &content)
                            && info.state == 'Z'
                        {
                            zombies.push(info);
                        }
                    }
                }
            }

            zombies
        }

        #[cfg(not(unix))]
        {
            Vec::new()
        }
    }

    /// 注册一个子进程（未来扩展）
    #[allow(dead_code)]
    pub fn register(&self, _child: Child) {
        // 当前实现中，子进程由各自的创建者负责回收
        // 此方法保留用于未来中央化管理的扩展
    }
}

impl Default for ProcessReaperHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::time::Duration;

    #[tokio::test]
    async fn test_reaper_state() {
        let config = ReaperConfig {
            verbose: true,
            ..Default::default()
        };
        let mut state = ReaperState::new(config);

        // 创建一个长时间运行的进程
        let child = tokio::process::Command::new("sleep")
            .arg("10")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let pid = child.id().unwrap();
        state.register_child(child);

        // 立即调用 reap_all，进程应该还在运行
        state.reap_all();

        // 验证进程仍在列表中（因为还没退出）
        assert!(state.active_children.contains_key(&pid));
        assert_eq!(state.reaped_count, 0); // 还没有回收任何进程

        // 清理：杀死进程
        if let Some(mut child) = state.active_children.remove(&pid) {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    #[tokio::test]
    async fn test_long_running_process() {
        let config = ReaperConfig::default();
        let mut state = ReaperState::new(config);

        // 创建一个长时间运行的进程
        let child = tokio::process::Command::new("sleep")
            .arg("10")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let pid = child.id().unwrap();
        state.register_child(child);

        // 立即尝试回收，进程应该还在运行
        state.reap_all();

        // 验证进程仍在列表中
        assert!(state.active_children.contains_key(&pid));

        // 清理：杀死进程
        if let Some(mut child) = state.active_children.remove(&pid) {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    #[tokio::test]
    async fn test_start_process_reaper() {
        let handle = start_process_reaper();

        // 创建几个快速退出的子进程
        for _ in 0..3 {
            let _ = tokio::process::Command::new("true")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }

        // 等待回收器处理
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 回收器应该仍在运行
        assert!(!handle.is_finished());

        // 取消任务
        handle.abort();
    }

    #[test]
    fn test_parse_stat_file() {
        let content = "1 (init) S 0 0 0 0 -1 4194560 667 5569406 8 23660837 1 0 0 0 0 0 0 0 20 0 1 0 3642608 1340 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        let info = parse_stat_file(1, content).unwrap();

        assert_eq!(info.pid, 1);
        assert_eq!(info.ppid, 0);
        assert_eq!(info.comm, "init");
        assert_eq!(info.state, 'S');
    }

    #[test]
    fn test_parse_stat_file_with_parentheses_in_comm() {
        // 进程名包含括号的情况
        let content = "1234 (test(a)b)) Z 1 1234 1234 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        let info = parse_stat_file(1234, content).unwrap();

        assert_eq!(info.pid, 1234);
        assert_eq!(info.ppid, 1);
        assert_eq!(info.comm, "test(a)b)");
        assert_eq!(info.state, 'Z'); // 僵尸进程
    }
}
