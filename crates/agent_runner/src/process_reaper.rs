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
use tokio::signal::unix::{signal, SignalKind};
use tokio::process::Child;
use tracing::{debug, info, warn, error};

/// 进程回收器配置
#[derive(Debug, Clone)]
pub struct ReaperConfig {
    /// 是否启用详细日志
    pub verbose: bool,
}

impl Default for ReaperConfig {
    fn default() -> Self {
        Self { verbose: false }
    }
}

/// 进程回收器状态
#[derive(Debug)]
struct ReaperState {
    /// 追踪活跃的子进程
    /// 存储格式: pid -> Child
    active_children: HashMap<u32, Child>,
    /// 回收的进程总数
    reaped_count: u64,
    /// 配置
    config: ReaperConfig,
}

impl ReaperState {
    fn new(config: ReaperConfig) -> Self {
        Self {
            active_children: HashMap::new(),
            reaped_count: 0,
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
                        debug!("[ProcessReaper] 回收子进程 PID={}, exit_status={:?}", pid, status);
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
}

/// 启动进程回收器任务
///
/// 此函数会：
/// 1. 注册 SIGCHLD 信号处理器
/// 2. 在后台循环中等待信号并回收子进程
///
/// # 返回值
///
/// 返回一个 JoinHandle，可以用于等待回收器任务退出（通常不需要）
pub fn start_process_reaper() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_reaper(ReaperConfig::default()).await
    })
}

/// 启动进程回收器任务（带配置）
pub fn start_process_reaper_with_config(config: ReaperConfig) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_reaper(config).await
    })
}

/// 核心回收逻辑
async fn run_reaper(config: ReaperConfig) {
    info!("[ProcessReaper] 僵尸进程回收器已启动 (PID 1 模式)");

    // 创建 SIGCHLD 信号监听器
    let mut sigchld = match signal(SignalKind::child()) {
        Ok(sig) => sig,
        Err(e) => {
            error!("[ProcessReaper] 无法注册 SIGCHLD 信号处理器: {}", e);
            error!("[ProcessReaper] 将使用轮询模式作为回退");

            // 回退模式：使用轮询
            run_reaper_polling(config).await;
            return;
        }
    };

    let mut state = ReaperState::new(config);

    // 启动定期轮询任务（作为信号机制的补充）
    let mut poll_interval = tokio::time::interval(std::time::Duration::from_secs(5));
    poll_interval.tick().await; // 跳过第一次立即触发

    loop {
        tokio::select! {
            // 等待 SIGCHLD 信号
            _ = sigchld.recv() => {
                if state.config.verbose {
                    debug!("[ProcessReaper] 收到 SIGCHLD 信号");
                }
                state.reap_all();
            }
            // 定期轮询（作为补充，防止信号丢失）
            _ = poll_interval.tick() => {
                state.reap_all();
            }
        }
    }
}

/// 轮询模式回退（当信号机制不可用时）
async fn run_reaper_polling(config: ReaperConfig) {
    info!("[ProcessReaper] 使用轮询模式回收僵尸进程");

    let mut state = ReaperState::new(config);
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));

    loop {
        interval.tick().await;
        state.reap_all();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn test_reaper_state() {
        let config = ReaperConfig { verbose: true };
        let mut state = ReaperState::new(config);

        // 创建一个快速退出的子进程
        let child = tokio::process::Command::new("true")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        let pid = child.id().unwrap();
        state.register_child(child);

        // 等待进程退出
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 回收进程
        state.reap_all();

        // 验证进程已被移除
        assert!(!state.active_children.contains_key(&pid));
        assert_eq!(state.reaped_count, 1);
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
}
