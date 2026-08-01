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

#![allow(dead_code)] // start_process_reaper_with_config / detect_zombies_now / register_child 为可选 API

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
                debug!("[ProcessReaper] Registered child process PID={}", id);
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
                            "[ProcessReaper] Reaped child process PID={}, exit_status={:?}",
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
                    warn!("[ProcessReaper] Failed to query child PID={}: {}", pid, e);
                    false // 移除无法查询的进程
                }
            }
        });

        if reaped_now > 0 {
            self.reaped_count += reaped_now;
            info!(
                "[ProcessReaper] Reaped {} child processes (total: {})",
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
                "[ProcessReaper] Detected {} zombie processes (total detected: {})",
                zombies.len(),
                self.zombie_detected_count
            );

            for zombie in &zombies {
                warn!(
                    "[ProcessReaper] Zombie process: PID={}, PPID={}, CMD={}",
                    zombie.pid, zombie.ppid, zombie.comm
                );
            }
        }

        zombies
    }

    /// 🔧 主动回收真·孤儿僵尸进程。
    ///
    /// PID 1 的责任: 回收被 reparent 到 PID 1 的孤儿僵尸(如 chromium/VNC 的孙进程)。
    ///
    /// **不再用 `waitpid(-1)`** —— 它无法区分孤儿与 `tokio::process` 正在 `child.wait()` 的
    /// 直系子进程, 会抢走 tokio 的子进程导致 `child.wait()` 拿到 ECHILD ("No child processes")。
    /// `tokio::process` 与同一进程内的 `waitpid(-1)` 是已知冲突反模式。
    ///
    /// 改用 /proc 扫描定位僵尸, 只回收 **PPID==1(归 PID 1)且非 tokio 拥有** 的, 按具体 PID
    /// `waitpid(pid)` 回收。tokio 拥有的 PID(已登记在 [`reaper_coord`] 注册表)留给 tokio 自己回收。
    fn reap_all_zombies_blocking(&mut self) {
        #[cfg(unix)]
        {
            use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
            use nix::unistd::Pid;

            // 1. 一次 /proc 扫描: 拿僵尸列表 + 当前全部存活 PID(用于清理注册表死 PID)。
            let (zombies, live_pids) = scan_proc_zombies_and_live();

            // 2. 清理 reaper_coord 注册表中已不在 /proc 的 PID —— 进程已退出且被 tokio 回收。
            //    避免 PID 复用: 否则一个已退出的 tokio 子进程 PID 被复用成新孤儿时,
            //    reaper 会误判为 tokio 拥有而漏回收。
            shared_types::reaper_coord::prune_dead_pids(&live_pids);

            // 3. 回收真·孤儿: PPID==1 且非 tokio 拥有的僵尸。按具体 PID 回收, 绝不 waitpid(-1)。
            let mut reaped_this_round = 0u64;
            for z in &zombies {
                if z.ppid != 1 {
                    continue; // 非 PID 1 的子进程, 其父进程负责回收
                }
                if shared_types::reaper_coord::is_tokio_owned(z.pid) {
                    continue; // tokio 拥有, 留给 tokio::process child.wait()
                }
                match waitpid(Pid::from_raw(z.pid as i32), Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::Exited(..)) | Ok(WaitStatus::Signaled(..)) => {
                        reaped_this_round += 1;
                        debug!(
                            "[ProcessReaper] Reaped orphan zombie: PID={}, cmd={}",
                            z.pid, z.comm
                        );
                    }
                    Ok(WaitStatus::StillAlive) | Ok(_) => {
                        // 还没真正退出 / 停止·恢复等非退出事件, 下轮再说
                    }
                    Err(nix::errno::Errno::ECHILD) => {
                        // 已被回收(如 tokio 刚收), 跳过
                    }
                    Err(e) => {
                        warn!("[ProcessReaper] waitpid({}) error: {}", z.pid, e);
                    }
                }
            }

            if reaped_this_round > 0 {
                self.reaped_count += reaped_this_round;
                info!(
                    "[ProcessReaper] Reaped {} orphan zombies (total: {})",
                    reaped_this_round, self.reaped_count
                );
            }
        }

        #[cfg(not(unix))]
        {
            debug!("[ProcessReaper] Non-Unix platform, skipping zombie detection");
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

/// 一次 /proc 扫描, 同时返回: (僵尸进程列表, 当前全部存活 PID 集合)。
///
/// 僵尸列表供 `reap_all_zombies_blocking` 按 PID 回收; 存活 PID 集合供
/// `reaper_coord::prune_dead_pids` 清理注册表中已退出的 tokio 子进程 PID。
/// 合并成一次 /proc 遍历, 避免重复扫描。
fn scan_proc_zombies_and_live() -> (Vec<ZombieProcessInfo>, std::collections::HashSet<u32>) {
    let mut zombies = Vec::new();
    let mut live = std::collections::HashSet::new();

    #[cfg(unix)]
    {
        if let Ok(entries) = fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(pid) = name.to_string_lossy().parse::<u32>().ok() else {
                    continue;
                };
                live.insert(pid);
                if let Ok(content) = fs::read_to_string(entry.path().join("stat"))
                    && let Some(info) = parse_stat_file(pid, &content)
                    && info.state == 'Z'
                {
                    zombies.push(info);
                }
            }
        }
    }

    if !zombies.is_empty() {
        warn!(
            "[ProcessReaper] Detected {} zombie processes (total detected: {})",
            zombies.len(),
            zombies.len()
        );
        for z in &zombies {
            warn!(
                "[ProcessReaper] Zombie process: PID={}, PPID={}, CMD={}",
                z.pid, z.ppid, z.comm
            );
        }
    }

    (zombies, live)
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
    info!("[ProcessReaper] Zombie process reaper started (PID 1 mode)");
    if config.enable_zombie_detection {
        info!(
            "[ProcessReaper] Zombie detection enabled, interval: {} seconds",
            config.zombie_detection_interval_secs
        );
    }

    // 创建 SIGCHLD 信号监听器
    let sigchld = match signal(SignalKind::child()) {
        Ok(sig) => sig,
        Err(e) => {
            error!("[ProcessReaper] Failed to register SIGCHLD handler: {}", e);
            error!("[ProcessReaper] Falling back to polling mode");

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
    info!("[ProcessReaper] Non-Unix platform: zombie reaper not applicable");
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
            // SIGCHLD: 只 reap_all(已注册子进程, cheap try_wait)。
            // 不在此处 reap_all_zombies_blocking —— 它会扫 /proc, 而 build 期间 SIGCHLD 极多
            // (每个编译进程退出都发一个), 逐信号扫 /proc 会拖慢 build。孤儿回收交给定时分支。
            _ = sigchld.recv() => {
                if state.config.verbose {
                    debug!("[ProcessReaper] Received SIGCHLD");
                }
                state.reap_all();
            }
            // 定期轮询（每 5 秒）—— 只 reap 已注册子进程
            _ = poll_interval.tick() => {
                state.reap_all();
            }
            // 🔍 定期主动回收孤儿僵尸(reap_all_zombies_blocking 内部一次 /proc 扫描:
            // 找 PPID==1 僵尸 + 跳过 tokio 拥有 + 按具体 PID 回收 + 清理注册表死 PID)。
            _ = zombie_detect_interval.tick() => {
                debug!("[ProcessReaper] Running scheduled zombie detection...");
                state.reap_all_zombies_blocking();
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
            // SIGCHLD: 只 reap_all(cheap, 不扫 /proc —— 见 with_detection 注释)
            _ = sigchld.recv() => {
                if state.config.verbose {
                    debug!("[ProcessReaper] Received SIGCHLD");
                }
                state.reap_all();
            }
            // 定期轮询（每 5 秒）—— 此模式无独立 zombie_detect 定时, 故在此处顺带回收孤儿
            _ = poll_interval.tick() => {
                state.reap_all();
                state.reap_all_zombies_blocking();
            }
        }
    }
}

/// 轮询模式回退（当信号机制不可用时）
#[cfg(unix)]
async fn run_reaper_polling(config: ReaperConfig) {
    info!("[ProcessReaper] Using polling mode for zombie reaping");

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
            }
            _ = zombie_detect_interval.tick() => {
                state.reap_all_zombies_blocking();
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
    ///
    /// ⚠️ 当前实现为空操作，调用方需注意子进程不会被自动追踪和回收。
    /// 子进程仍由其创建者负责管理。
    #[allow(dead_code)]
    pub fn register(&self, _child: Child) {
        warn!(
            "[ProcessReaper] register() called but not implemented - \
             child process will not be tracked. \
             Caller should manage the child process lifecycle independently."
        );
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
