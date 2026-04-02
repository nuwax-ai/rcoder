//! Agent Runtime 模块
//!
//! 简化版的 Agent Worker 管理器，利用 SACP 的 Send trait 支持。
//!
//! ## 新架构设计
//!
//! - 移除独立 OS 线程，使用 `tokio::spawn`
//! - 简化 sender 管理，移除 ArcSwap
//! - 保留自动重启功能
//! - 保留心跳检测（僵尸检测）
//! - 简化状态机（使用原子操作）
//!
//! ## 与旧架构对比
//!
//! | 组件 | 旧设计 | 新设计 |
//! |------|--------|--------|
//! | 运行环境 | 独立 OS 线程 + 独立运行时 | 主运行时 + tokio::spawn |
//! | Sender 管理 | ArcSwap<Option<Sender>> | mpsc::Sender (固定) |
//! | Worker 生命周期 | 手动管理线程 | JoinHandle |
//! | Ready 信号 | oneshot::channel | JoinHandle 完成 |
//! | 状态机 | watch::Sender + Mutex | Arc<AtomicState> |
//! | 重启 | 替换 sender | abort + spawn |

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::proxy_agent::AgentRequest;

/// Worker 状态
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    /// 启动中
    Starting = 0,
    /// 运行中
    Running = 1,
    /// 停止中
    Stopping = 2,
    /// 已停止
    Stopped = 3,
}

/// 原子状态包装器 (无需 Mutex)
pub struct AtomicState(AtomicU8);

impl AtomicState {
    pub fn new(state: WorkerState) -> Self {
        Self(AtomicU8::new(state as u8))
    }

    pub fn get(&self) -> WorkerState {
        match self.0.load(Ordering::Acquire) {
            0 => WorkerState::Starting,
            1 => WorkerState::Running,
            2 => WorkerState::Stopping,
            3 => WorkerState::Stopped,
            invalid => {
                tracing::error!(
                    "[AtomicState] Invalid state value: {}, falling back to Stopped",
                    invalid
                );
                WorkerState::Stopped
            }
        }
    }

    pub fn set(&self, state: WorkerState) {
        self.0.store(state as u8, Ordering::Release);
    }
}

/// 心跳包
#[derive(Clone, Debug)]
pub struct Heartbeat {
    /// 心跳时间戳
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Worker 就绪信号
///
/// Worker 在初始化完成后发送此信号
#[derive(Clone, Debug)]
pub struct WorkerReady {
    /// 就绪时间戳
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// 并发控制配置
///
/// 工作线程池大小 - 决定可以并发处理的 Agent 会话数量
/// 🔥 已改为运行时可配置的全局变量，使用 get_concurrency_limit() 获取

/// 全局并发限制（运行时可配置）
pub static WORKER_THREAD_POOL_SIZE: AtomicUsize = AtomicUsize::new(10);

/// 初始化并发限制（在应用启动时调用）
pub fn init_concurrency_limit(limit: usize) {
    WORKER_THREAD_POOL_SIZE.store(limit, Ordering::Release);
    info!("🔧 Concurrency limit initialized: {}", limit);
}

/// 获取当前并发限制
pub fn get_concurrency_limit() -> usize {
    WORKER_THREAD_POOL_SIZE.load(Ordering::Acquire)
}

/// Agent 运行时
///
/// 替代 AgentWorkerManager，使用简化的架构：
/// - 直接在主运行时中运行 (SACP 支持 Send)
/// - 使用原子操作管理状态
/// - 使用 JoinHandle 管理生命周期
pub struct AgentRuntime {
    /// 请求发送端 (固定不变)
    request_tx: mpsc::Sender<AgentRequest>,

    /// 当前 Worker 的 JoinHandle
    worker_handle: Arc<Mutex<Option<JoinHandle<()>>>>,

    /// 当前状态
    state: Arc<AtomicState>,

    /// 🔥 P1 修复: 最后心跳时间戳（毫秒，Unix timestamp）
    /// 使用 AtomicI64 替代 Mutex，避免频繁的锁竞争
    /// - 0 表示从未收到心跳
    /// - 正数表示最后一次心跳的 timestamp_millis()
    last_heartbeat_ts: Arc<AtomicI64>,

    /// 活跃请求追踪: request_id -> 开始时间
    active_requests: Arc<Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>>,

    /// 心跳超时阈值
    heartbeat_timeout: Duration,

    /// 首次启动宽限期
    initial_grace_period: Duration,
}

impl AgentRuntime {
    /// 创建新的 AgentRuntime
    ///
    /// 返回 (runtime, request_receiver)
    pub fn new(request_buffer: usize) -> (Self, mpsc::Receiver<AgentRequest>) {
        let (request_tx, request_rx) = mpsc::channel(request_buffer);

        let runtime = Self {
            request_tx,
            worker_handle: Arc::new(Mutex::new(None)),
            state: Arc::new(AtomicState::new(WorkerState::Starting)),
            last_heartbeat_ts: Arc::new(AtomicI64::new(0)),
            active_requests: Arc::new(Mutex::new(HashMap::new())),
            heartbeat_timeout: Duration::from_secs(15),
            initial_grace_period: Duration::from_secs(30),
        };

        (runtime, request_rx)
    }

    /// 启动 Worker (在主运行时中)
    ///
    /// SACP 支持 Send，直接在主运行时中运行，无需独立线程
    pub async fn start(&self, receiver: mpsc::Receiver<AgentRequest>) {
        let state = self.state.clone();
        let last_heartbeat_ts = self.last_heartbeat_ts.clone();
        let active_requests = self.active_requests.clone();

        let handle = tokio::spawn(async move {
            // SACP 支持 Send，直接在主运行时中运行
            if let Err(e) = crate::proxy_agent::agent_worker_with_heartbeat(
                receiver,
                state.clone(),
                last_heartbeat_ts.clone(),
                active_requests.clone(),
            )
            .await
            {
                tracing::error!("Agent worker failed: {}", e);
            }
        });

        *self.worker_handle.lock().await = Some(handle);
        self.state.set(WorkerState::Running);
        info!("AgentRuntime: worker started");
    }

    /// 重启 Worker
    pub async fn restart(&self, new_receiver: mpsc::Receiver<AgentRequest>) {
        warn!("AgentRuntime: preparing to restart worker...");

        // 1. 停止旧 worker
        if let Some(handle) = self.worker_handle.lock().await.take() {
            handle.abort();
            info!("AgentRuntime: previous worker terminated");
        }

        // 2. 重置状态
        self.state.set(WorkerState::Starting);
        self.last_heartbeat_ts.store(0, Ordering::Release);
        *self.active_requests.lock().await = HashMap::new();

        // 3. 启动新 worker
        self.start(new_receiver).await;
        info!("AgentRuntime: worker restart completed");
    }

    /// 发送请求
    pub async fn send(&self, request: AgentRequest) -> anyhow::Result<()> {
        self.request_tx
            .send(request)
            .await
            .map_err(|_| anyhow::anyhow!("Worker is closed"))?;
        Ok(())
    }

    /// 健康检查
    pub async fn check_health(&self) -> bool {
        let state = self.state.get();

        // 检查状态
        if state == WorkerState::Stopped {
            return false;
        }

        // 🔥 P1 修复: 使用原子操作检查心跳（无锁）
        let last_ts = self.last_heartbeat_ts.load(Ordering::Acquire);
        if last_ts > 0 {
            let elapsed_ms = Utc::now().timestamp_millis() - last_ts;
            elapsed_ms < self.heartbeat_timeout.as_millis() as i64
        } else {
            // 首次启动宽限期
            true
        }
    }

    /// 获取当前状态
    pub fn state(&self) -> WorkerState {
        self.state.get()
    }

    /// 🔥 P1 修复: 检查心跳是否超时（无锁）
    ///
    /// ## 返回值
    ///
    /// - `true`: 心跳超时（超过 15 秒未收到心跳）
    /// - `false`: 心跳正常或在宽限期内
    pub fn check_heartbeat_timeout(&self) -> bool {
        let last_ts = self.last_heartbeat_ts.load(Ordering::Acquire);

        if last_ts > 0 {
            // 有心跳记录，检查是否超过 15 秒
            let elapsed_ms = Utc::now().timestamp_millis() - last_ts;
            elapsed_ms > 15_000
        } else {
            // 从未收到心跳，使用首次启动宽限期
            false
        }
    }

    /// 🔥 P1 修复: 获取最后心跳时间（无锁）
    ///
    /// ## 返回值
    ///
    /// - `Some(timestamp)`: 最后心跳时间
    /// - `None`: 从未收到心跳
    pub fn last_heartbeat_time(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let last_ts = self.last_heartbeat_ts.load(Ordering::Acquire);
        if last_ts > 0 {
            // 将毫秒时间戳转换为 DateTime
            use chrono::TimeZone;
            // timestamp_millis_opt 返回 LocalResult，使用 single() 转换为 Option
            match chrono::Utc.timestamp_millis_opt(last_ts).single() {
                Some(dt) => Some(dt),
                None => {
                    tracing::warn!(
                        "[WorkerInfo] Invalid timestamp: {}, using current time",
                        last_ts
                    );
                    Some(chrono::Utc::now())
                }
            }
        } else {
            None
        }
    }

    /// 获取活跃请求句柄
    pub fn active_requests(&self) -> Arc<Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>> {
        self.active_requests.clone()
    }

    /// 检查请求通道是否已关闭
    pub fn is_closed(&self) -> bool {
        self.request_tx.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_atomic_state() {
        let state = AtomicState::new(WorkerState::Starting);
        assert_eq!(state.get(), WorkerState::Starting);

        state.set(WorkerState::Running);
        assert_eq!(state.get(), WorkerState::Running);

        state.set(WorkerState::Stopped);
        assert_eq!(state.get(), WorkerState::Stopped);
    }

    #[tokio::test]
    async fn test_agent_runtime_creation() {
        let (runtime, _rx) = AgentRuntime::new(100);
        assert_eq!(runtime.state(), WorkerState::Starting);
        assert!(!runtime.is_closed());
    }

    #[tokio::test]
    async fn test_heartbeat_timeout_detection() {
        let (runtime, _rx) = AgentRuntime::new(100);

        // 初始状态：从未收到心跳
        assert!(!runtime.check_heartbeat_timeout());

        // 模拟心跳超时（设置20秒前的时间戳）
        let timestamp_20s_ago = Utc::now().timestamp_millis() - (20 * 1000);
        runtime
            .last_heartbeat_ts
            .store(timestamp_20s_ago, std::sync::atomic::Ordering::Release);

        // 心跳超过 15 秒，应检测到超时
        assert!(runtime.check_heartbeat_timeout());
    }

    #[tokio::test]
    async fn test_health_check() {
        let (runtime, _rx) = AgentRuntime::new(100);

        // 初始状态应该是健康的
        assert!(runtime.check_health().await);

        // 设置状态为 Stopped
        runtime.state.set(WorkerState::Stopped);
        assert!(!runtime.check_health().await);
    }
}
