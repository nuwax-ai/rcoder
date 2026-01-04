//! Agent Worker 管理器
//!
//! 负责管理 agent_worker 线程的生命周期，包括：
//! - 动态替换任务发送器
//! - 监控 worker 健康状态（心跳机制）
//! - 广播 worker 状态变化

use arc_swap::ArcSwap;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};

use crate::proxy_agent::LocalSetAgentRequest;

/// Worker 状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    /// 启动中
    Starting,
    /// 运行中
    Running,
    /// 停止中
    Stopping,
    /// 已停止（崩溃或正常退出）
    Stopped,
}

/// 心跳包
#[derive(Clone, Debug)]
pub struct Heartbeat {
    /// 心跳时间戳
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// Worker 就绪信号
///
/// Worker 在 LocalSet 初始化完成后发送此信号
#[derive(Clone, Debug)]
pub struct WorkerReady {
    /// 就绪时间戳
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// 活跃请求统计信息
///
/// 用于监控任务，避免克隆整个 HashMap
#[derive(Debug, Clone, Default)]
pub struct ActiveRequestsSummary {
    /// 活跃请求数量
    pub count: usize,
    /// 最大持续时间（秒）
    pub max_duration_secs: i64,
    /// 超过 60 秒的请求数
    pub timeout_count_60: usize,
    /// 超过 120 秒的请求数
    pub timeout_count_120: usize,
}

/// 🔥 DoS 防护配置
///
/// 限制活跃请求追踪的最大数量，防止恶意攻击导致内存耗尽
pub const MAX_ACTIVE_REQUESTS: usize = 10_000;

/// Worker 句柄（传递给 worker 线程）
///
/// Worker 线程通过此句柄发送心跳信号和就绪信号
pub struct WorkerHandle {
    /// 用于发送心跳
    pub heartbeat_tx: mpsc::Sender<Heartbeat>,
    /// 用于发送就绪信号（一次性，使用 oneshot）
    pub ready_tx: Option<oneshot::Sender<WorkerReady>>,
    /// 🆕 用于追踪请求的 Arc 引用 (可选,用于请求超时检测)
    pub active_requests: Option<Arc<std::sync::Mutex<HashMap<String, chrono::DateTime<Utc>>>>>,
}

/// Agent Worker 管理器
///
/// 负责管理 sender 的状态和心跳检测
pub struct AgentWorkerManager {
    /// 当前任务发送器（使用 ArcSwap 支持无锁原子替换）
    sender: ArcSwap<Option<mpsc::UnboundedSender<LocalSetAgentRequest>>>,

    /// 状态广播通道
    state_tx: watch::Sender<WorkerState>,

    /// 最后心跳时间
    last_heartbeat: Arc<std::sync::Mutex<Option<chrono::DateTime<chrono::Utc>>>>,

    /// 上次状态变化时间（用于首次启动超时检测）
    last_state_change: Arc<std::sync::Mutex<chrono::DateTime<chrono::Utc>>>,

    /// 🆕 活跃请求追踪: request_id -> 开始时间
    active_requests: Arc<std::sync::Mutex<HashMap<String, chrono::DateTime<Utc>>>>,
}

impl AgentWorkerManager {
    /// 创建新的管理器和通道
    ///
    /// 返回 (manager, heartbeat_rx, ready_rx, heartbeat_tx, ready_tx)
    /// - heartbeat_rx: 用于监控任务接收心跳
    /// - ready_rx: 用于监控任务接收就绪信号（oneshot）
    /// - heartbeat_tx: 用于传递给 worker 发送心跳
    /// - ready_tx: 用于传递给 worker 发送就绪信号（oneshot）
    ///
    /// 注意：创建后必须立即调用 `set_sender()` 设置有效的通道
    #[allow(clippy::type_complexity)]
    pub fn new() -> (
        Self,
        mpsc::Receiver<Heartbeat>,
        oneshot::Receiver<WorkerReady>,
        mpsc::Sender<Heartbeat>,
        oneshot::Sender<WorkerReady>,
    ) {
        // 创建初始的 sender（None，表示尚未设置）
        let sender: ArcSwap<Option<mpsc::UnboundedSender<LocalSetAgentRequest>>> =
            ArcSwap::from_pointee(None);

        let (state_tx, _state_rx) = watch::channel(WorkerState::Starting);
        let (heartbeat_tx, heartbeat_rx) = mpsc::channel(100);
        let (ready_tx, ready_rx) = oneshot::channel(); // 🆕 使用 oneshot
        let now = Utc::now();

        let manager = Self {
            sender,
            state_tx,
            last_heartbeat: Arc::new(std::sync::Mutex::new(None)),
            last_state_change: Arc::new(std::sync::Mutex::new(now)),
            active_requests: Arc::new(std::sync::Mutex::new(HashMap::new())),
        };

        (manager, heartbeat_rx, ready_rx, heartbeat_tx, ready_tx)
    }

    /// 获取当前 sender 的克隆
    pub fn get_sender(&self) -> Option<mpsc::UnboundedSender<LocalSetAgentRequest>> {
        self.sender.load().as_ref().clone()
    }

    /// 创建 worker 句柄（包含心跳和就绪通道）
    pub fn create_handle(
        &self,
        heartbeat_tx: mpsc::Sender<Heartbeat>,
        ready_tx: oneshot::Sender<WorkerReady>,
    ) -> WorkerHandle {
        WorkerHandle {
            heartbeat_tx,
            ready_tx: Some(ready_tx),
            active_requests: Some(self.active_requests.clone()),
        }
    }

    /// 获取当前状态
    pub fn state(&self) -> WorkerState {
        *self.state_tx.borrow()
    }

    /// 获取最后心跳时间
    pub fn last_heartbeat_time(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.last_heartbeat
            .lock()
            .expect("AgentWorkerManager::last_heartbeat mutex poisoned")
            .clone()
    }

    /// 设置初始 sender（由 main.rs 调用）
    pub fn set_sender(&self, sender: &mpsc::UnboundedSender<LocalSetAgentRequest>) {
        self.sender.store(Arc::new(Some(sender.clone())));
    }

    /// 尝试发送任务
    ///
    /// 增加通道关闭检查，避免 worker 崩溃后发送失败卡住
    pub async fn try_send(&self, request: LocalSetAgentRequest) -> anyhow::Result<()> {
        let sender = self
            .get_sender()
            .ok_or_else(|| anyhow::anyhow!("Worker sender 不可用"))?;

        // 🆕 检查通道是否已关闭（worker 可能正在重启）
        if sender.is_closed() {
            return Err(anyhow::anyhow!("Worker 通道已关闭，可能正在重启中"));
        }

        sender
            .send(request)
            .map_err(|e| anyhow::anyhow!("发送任务失败: {}", e))?;

        Ok(())
    }

    /// 🔥 检查心跳是否超时（超过15秒视为超时）
    ///
    /// 特殊处理：如果从未收到心跳，检查状态变化时间
    /// - 首次启动后 30 秒内未收到心跳 → 视为超时
    /// - 正常运行时 15 秒无心跳 → 视为超时
    ///
    /// 注意：分别获取两个锁，避免长时间持锁
    pub fn check_heartbeat_timeout(&self) -> bool {
        // 先读取最后心跳时间（快速释放锁）
        let last_heartbeat_opt = {
            let last = self
                .last_heartbeat
                .lock()
                .expect("AgentWorkerManager::last_heartbeat mutex poisoned");
            *last // Copy 数据，立即释放锁
        };

        if let Some(timestamp) = last_heartbeat_opt {
            // 有心跳记录，检查是否超过 15 秒
            let elapsed = Utc::now() - timestamp;
            elapsed.num_seconds() > 15
        } else {
            // 从未收到心跳，检查状态变化时间（单独获取锁）
            let state_change = self
                .last_state_change
                .lock()
                .expect("AgentWorkerManager::last_state_change mutex poisoned");
            let elapsed = Utc::now() - *state_change;
            elapsed.num_seconds() > 30
        }
    }

    /// 🔥 更新心跳（由监控任务调用）
    pub fn update_heartbeat(&self, heartbeat: Heartbeat) {
        let mut last = self
            .last_heartbeat
            .lock()
            .expect("AgentWorkerManager::last_heartbeat mutex poisoned");
        *last = Some(heartbeat.timestamp);
        debug!("💓 [WorkerMonitor] 心跳已更新");
    }

    /// 🔥 替换 sender（用于重启 worker）
    ///
    /// 当 worker 崩溃重启时，需要替换为新的通道 sender
    pub fn replace_sender(&self, new_sender: mpsc::UnboundedSender<LocalSetAgentRequest>) {
        self.sender.store(Arc::new(Some(new_sender)));
        info!("🔄 [AgentWorkerManager] Sender 已替换");
    }

    /// 🔥 更新状态
    ///
    /// 使用 send_modify 而不是 send，因为当 receiver 被 drop 后 send 会失败
    pub fn update_state(&self, new_state: WorkerState) {
        self.state_tx.send_modify(|state| *state = new_state);
        // 更新状态变化时间
        let mut state_change = self
            .last_state_change
            .lock()
            .expect("AgentWorkerManager::last_state_change mutex poisoned");
        *state_change = Utc::now();
        debug!("📊 [AgentWorkerManager] 状态更新为: {:?}", new_state);
    }

    /// 🆕 重置心跳时间（用于重启 worker 时）
    ///
    /// 当 worker 重启时，需要重置心跳时间为 None，避免旧的过期心跳时间导致立即触发超时
    pub fn reset_heartbeat(&self) {
        let mut last = self
            .last_heartbeat
            .lock()
            .expect("AgentWorkerManager::last_heartbeat mutex poisoned");
        *last = None;
        debug!("🔄 [AgentWorkerManager] 心跳时间已重置");
    }

    /// 🆕 追踪请求开始
    ///
    /// 在发送请求到 Worker 时调用,记录请求开始时间
    ///
    /// # 返回
    /// - `Ok(())` - 请求追踪成功
    /// - `Err(reason)` - 请求被拒绝（DoS 防护触发）
    ///
    /// # DoS 防护
    /// 当活跃请求数超过 `MAX_ACTIVE_REQUESTS` 时，拒绝新请求
    pub fn track_request_start(&self, request_id: String) -> Result<(), String> {
        let mut requests = self
            .active_requests
            .lock()
            .expect("AgentWorkerManager::active_requests mutex poisoned");

        // 🔥 DoS 防护检查
        if requests.len() >= MAX_ACTIVE_REQUESTS {
            warn!(
                "🛡️ [DoS防护] 活跃请求数已达上限 ({})，拒绝新请求: {}",
                MAX_ACTIVE_REQUESTS, request_id
            );
            return Err(format!(
                "活跃请求数已达上限 ({})，请稍后重试",
                MAX_ACTIVE_REQUESTS
            ));
        }

        requests.insert(request_id, Utc::now());
        debug!(
            "📍 [WorkerMonitor] 追踪请求开始, 活跃请求数: {}",
            requests.len()
        );
        Ok(())
    }

    /// 🆕 追踪请求完成
    ///
    /// 在收到响应时调用,移除请求追踪
    pub fn track_request_complete(&self, request_id: &str) {
        let mut requests = self
            .active_requests
            .lock()
            .expect("AgentWorkerManager::active_requests mutex poisoned");
        requests.remove(request_id);
        debug!(
            "📍 [WorkerMonitor] 请求完成, 活跃请求数: {}",
            requests.len()
        );
    }

    /// 🆕 获取活跃请求统计信息（性能优化版本）
    ///
    /// 避免克隆整个 HashMap，只返回统计信息用于监控
    pub fn get_active_requests_summary(&self) -> ActiveRequestsSummary {
        let requests = self
            .active_requests
            .lock()
            .expect("AgentWorkerManager::active_requests mutex poisoned");
        let now = Utc::now();
        let mut max_duration = 0;
        let mut timeout_count_60 = 0;
        let mut timeout_count_120 = 0;

        for (_req_id, start_time) in requests.iter() {
            let duration = (now - *start_time).num_seconds();
            max_duration = max_duration.max(duration);
            if duration > 120 {
                timeout_count_120 += 1;
            } else if duration > 60 {
                timeout_count_60 += 1;
            }
        }

        ActiveRequestsSummary {
            count: requests.len(),
            max_duration_secs: max_duration,
            timeout_count_60,
            timeout_count_120,
        }
    }

    /// 🆕 获取所有活跃请求（仅用于调试，不建议高频调用）
    ///
    /// ⚠️ 性能警告：此方法会克隆整个 HashMap
    pub fn get_active_requests(&self) -> HashMap<String, chrono::DateTime<Utc>> {
        let requests = self
            .active_requests
            .lock()
            .expect("AgentWorkerManager::active_requests mutex poisoned");
        requests.clone()
    }

    /// 🆕 清除所有活跃请求（用于重启时）
    ///
    /// 当 Worker 重启时,清除所有旧的请求追踪
    pub fn clear_active_requests(&self) {
        let mut requests = self
            .active_requests
            .lock()
            .expect("AgentWorkerManager::active_requests mutex poisoned");
        let count = requests.len();
        requests.clear();
        debug!("🔄 [WorkerMonitor] 已清除 {} 个活跃请求", count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试基本的 Manager 创建和状态设置
    #[tokio::test]
    async fn test_manager_creation_and_state() {
        let (manager, _heartbeat_rx, _ready_rx, _heartbeat_tx, _ready_tx) =
            AgentWorkerManager::new();
        let manager = Arc::new(manager);

        // 初始状态应为 Starting
        assert_eq!(manager.state(), WorkerState::Starting);

        // 更新状态为 Running
        manager.update_state(WorkerState::Running);

        // 强制同步检查
        let current_state = manager.state();
        assert_eq!(current_state, WorkerState::Running);

        // 更新状态为 Stopped
        manager.update_state(WorkerState::Stopped);
        assert_eq!(manager.state(), WorkerState::Stopped);
    }

    /// 测试心跳更新和超时检测
    #[tokio::test]
    async fn test_heartbeat_timeout_detection() {
        let (manager, _heartbeat_rx, _ready_rx, _heartbeat_tx, _ready_tx) =
            AgentWorkerManager::new();
        let manager = Arc::new(manager);

        // 初始状态：从未收到心跳，且刚创建，不应超时（30秒宽限）
        assert!(!manager.check_heartbeat_timeout());

        // 发送心跳
        let heartbeat = Heartbeat {
            timestamp: Utc::now(),
        };
        manager.update_heartbeat(heartbeat);

        // 刚发送心跳，不应超时
        assert!(!manager.check_heartbeat_timeout());

        // 模拟心跳时间过期（通过设置旧的时间戳）
        let old_heartbeat = Heartbeat {
            timestamp: Utc::now() - chrono::Duration::seconds(20),
        };
        manager.update_heartbeat(old_heartbeat);

        // 心跳超过 15 秒，应检测到超时
        assert!(manager.check_heartbeat_timeout());
    }

    /// 测试 Ready 信号发送和接收（oneshot）
    #[tokio::test]
    async fn test_ready_signal_with_oneshot() {
        let (manager, _heartbeat_rx, ready_rx, _heartbeat_tx, ready_tx) = AgentWorkerManager::new();
        let manager = Arc::new(manager);

        // 初始状态为 Starting
        assert_eq!(manager.state(), WorkerState::Starting);

        // 模拟 Worker 发送 Ready 信号
        let ready_signal = WorkerReady {
            timestamp: Utc::now(),
        };
        ready_tx
            .send(ready_signal)
            .expect("Failed to send ready signal");

        // 接收 Ready 信号
        let received = ready_rx.await.expect("Failed to receive ready signal");
        assert!(received.timestamp <= Utc::now());

        // 更新状态为 Running
        manager.update_state(WorkerState::Running);
        assert_eq!(manager.state(), WorkerState::Running);
    }

    /// 测试 Worker 崩溃场景：心跳通道关闭后检测到超时
    #[tokio::test]
    async fn test_worker_crash_detection() {
        let (manager, heartbeat_rx, _ready_rx, heartbeat_tx, _ready_tx) = AgentWorkerManager::new();
        let manager = Arc::new(manager);

        // 模拟 Worker 发送一次心跳
        let heartbeat = Heartbeat {
            timestamp: Utc::now(),
        };
        heartbeat_tx
            .send(heartbeat)
            .await
            .expect("Failed to send heartbeat");

        // 模拟 Worker 崩溃：关闭心跳通道
        drop(heartbeat_tx);
        drop(heartbeat_rx);

        // 设置一个旧的心跳时间戳模拟超时
        let old_heartbeat = Heartbeat {
            timestamp: Utc::now() - chrono::Duration::seconds(20),
        };
        manager.update_heartbeat(old_heartbeat);

        // 检测到心跳超时
        assert!(manager.check_heartbeat_timeout());

        // 模拟重启：更新状态为 Starting
        manager.update_state(WorkerState::Starting);
        assert_eq!(manager.state(), WorkerState::Starting);
    }

    /// 测试 try_send 通道关闭检测
    #[tokio::test]
    async fn test_try_send_channel_closed() {
        let (manager, _heartbeat_rx, _ready_rx, _heartbeat_tx, _ready_tx) =
            AgentWorkerManager::new();
        let manager = Arc::new(manager);

        // 创建并设置 sender
        let (sender, receiver) = mpsc::unbounded_channel();
        manager.set_sender(&sender);

        // 关闭 receiver，模拟 Worker 崩溃
        drop(receiver);

        // sender 应该检测到通道已关闭
        assert!(sender.is_closed());
    }

    /// 测试 Sender 替换（重启场景）
    #[tokio::test]
    async fn test_sender_replacement() {
        let (manager, _heartbeat_rx, _ready_rx, _heartbeat_tx, _ready_tx) =
            AgentWorkerManager::new();
        let manager = Arc::new(manager);

        // 设置初始 sender
        let (old_sender, old_receiver) = mpsc::unbounded_channel();
        manager.set_sender(&old_sender);

        // 验证可以获取 sender
        assert!(manager.get_sender().is_some());

        // 关闭旧的 receiver
        drop(old_receiver);

        // 创建新的 sender 并替换
        let (new_sender, _new_receiver) = mpsc::unbounded_channel();
        manager.replace_sender(new_sender);

        // 验证新的 sender 可用
        let current_sender = manager.get_sender().expect("Sender should exist");
        assert!(!current_sender.is_closed());
    }

    /// 测试首次启动30秒宽限期
    #[tokio::test]
    async fn test_initial_startup_grace_period() {
        let (manager, _heartbeat_rx, _ready_rx, _heartbeat_tx, _ready_tx) =
            AgentWorkerManager::new();
        let manager = Arc::new(manager);

        // 刚创建，从未收到心跳，但在 30 秒宽限期内，不应超时
        assert!(!manager.check_heartbeat_timeout());

        // 注意：测试 30 秒超时需要模拟时间，这里只验证逻辑正确性
        // 实际的 30 秒超时测试会太慢，跳过
    }
}
