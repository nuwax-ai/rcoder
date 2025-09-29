//! Agent停止句柄
//!
//! 使用enum和CancellationToken提供统一的agent停止接口和具体实现

use anyhow::Result;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::model::AgentType;
use agent_client_protocol::{ClientSideConnection, SessionId};

/// Agent停止句柄枚举
#[derive(Clone)]
pub enum AgentStopHandle {
    /// Codex Agent停止句柄
    Codex(CodexAgentStopHandle),
    /// Claude Code Agent停止句柄
    Claude(ClaudeCodeAgentStopHandle),
}

impl AgentStopHandle {
    /// 取消agent服务
    pub fn cancel(&self) {
        match self {
            AgentStopHandle::Codex(handle) => handle.cancel(),
            AgentStopHandle::Claude(handle) => handle.cancel(),
        }
    }

    /// 停止agent服务（立即停止）
    pub async fn stop(self) -> Result<()> {
        match self {
            AgentStopHandle::Codex(handle) => handle.stop().await,
            AgentStopHandle::Claude(handle) => handle.stop().await,
        }
    }

    /// 获取agent类型
    pub fn agent_type(&self) -> AgentType {
        match self {
            AgentStopHandle::Codex(handle) => handle.agent_type(),
            AgentStopHandle::Claude(handle) => handle.agent_type(),
        }
    }

    /// 检查agent是否已经停止
    pub fn is_stopped(&self) -> bool {
        match self {
            AgentStopHandle::Codex(handle) => handle.is_stopped(),
            AgentStopHandle::Claude(handle) => handle.is_stopped(),
        }
    }

    /// 获取CancellationToken（用于任务协作取消）
    pub fn cancellation_token(&self) -> CancellationToken {
        match self {
            AgentStopHandle::Codex(handle) => handle.cancellation_token().clone(),
            AgentStopHandle::Claude(handle) => handle.cancellation_token().clone(),
        }
    }

    /// 协作式取消（先发送取消信号，等待一段时间后再强制停止）
    pub async fn stop_with_timeout(self, timeout: std::time::Duration) -> Result<()> {
        // 先发送取消信号
        self.cancel();

        // 等待一段时间让任务优雅退出
        tokio::time::sleep(timeout).await;

        // 如果仍然没有停止，则强制停止
        if !self.is_stopped() {
            self.stop().await?;
        }

        Ok(())
    }
}

/// Codex Agent停止句柄
#[derive(Clone)]
pub struct CodexAgentStopHandle {
    /// 取消令牌
    cancel_token: CancellationToken,
    /// ACP连接
    client_conn: Option<Arc<ClientSideConnection>>,
    /// 会话ID
    session_id: SessionId,
    /// IO任务句柄
    io_task_handles: Vec<Arc<Mutex<Option<JoinHandle<Result<(), anyhow::Error>>>>>>,
    /// 通道任务句柄
    channel_task_handles: Vec<Arc<Mutex<Option<JoinHandle<()>>>>>,
    /// 是否已经停止
    stopped: Arc<Mutex<bool>>,
}

impl CodexAgentStopHandle {
    /// 创建新的Codex Agent停止句柄
    pub fn new(
        client_conn: Arc<ClientSideConnection>,
        session_id: SessionId,
        io_task_handles: Vec<JoinHandle<Result<(), anyhow::Error>>>,
        channel_task_handles: Vec<JoinHandle<()>>,
    ) -> Self {
        let cancel_token = CancellationToken::new();
        let wrapped_io_handles = io_task_handles
            .into_iter()
            .map(|h| Arc::new(Mutex::new(Some(h))))
            .collect();

        let wrapped_channel_handles = channel_task_handles
            .into_iter()
            .map(|h| Arc::new(Mutex::new(Some(h))))
            .collect();

        Self {
            cancel_token,
            client_conn: Some(client_conn),
            session_id,
            io_task_handles: wrapped_io_handles,
            channel_task_handles: wrapped_channel_handles,
            stopped: Arc::new(Mutex::new(false)),
        }
    }

    /// 使用外部CancellationToken创建新的Codex Agent停止句柄
    pub fn with_cancellation_token(
        client_conn: Arc<ClientSideConnection>,
        session_id: SessionId,
        io_task_handles: Vec<JoinHandle<Result<(), anyhow::Error>>>,
        channel_task_handles: Vec<JoinHandle<()>>,
        cancel_token: CancellationToken,
    ) -> Self {
        let wrapped_io_handles = io_task_handles
            .into_iter()
            .map(|h| Arc::new(Mutex::new(Some(h))))
            .collect();

        let wrapped_channel_handles = channel_task_handles
            .into_iter()
            .map(|h| Arc::new(Mutex::new(Some(h))))
            .collect();

        Self {
            cancel_token,
            client_conn: Some(client_conn),
            session_id,
            io_task_handles: wrapped_io_handles,
            channel_task_handles: wrapped_channel_handles,
            stopped: Arc::new(Mutex::new(false)),
        }
    }

    /// 取消agent服务
    pub fn cancel(&self) {
        if !self.cancel_token.is_cancelled() {
            info!("发送取消信号到Codex Agent，会话ID: {}", self.session_id.0);
            self.cancel_token.cancel();
        }
    }

    /// 停止agent服务
    pub async fn stop(&self) -> Result<()> {
        let mut stopped_guard = self.stopped.lock().await;
        if *stopped_guard {
            info!("Codex Agent已经停止，无需重复停止");
            return Ok(());
        }

        // 先发送取消信号
        if !self.cancel_token.is_cancelled() {
            self.cancel_token.cancel();
        }

        info!("正在停止Codex Agent，会话ID: {}", self.session_id.0);

        // 1. 停止IO任务
        for handle in &self.io_task_handles {
            if let Some(task) = handle.lock().await.take() {
                debug!("停止Codex Agent IO任务");
                task.abort();
                // 等待任务结束
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }

        // 2. 停止通道任务
        for handle in &self.channel_task_handles {
            if let Some(task) = handle.lock().await.take() {
                debug!("停止Codex Agent通道任务");
                task.abort();
                // 等待任务结束
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }

        // 3. 关闭ACP连接（通过drop实现）
        if let Some(_conn) = &self.client_conn {
            debug!("关闭Codex Agent ACP连接");
            // ClientSideConnection会在drop时自动关闭连接
        }

        *stopped_guard = true;
        info!("Codex Agent停止完成");
        Ok(())
    }

    /// 获取agent类型
    pub fn agent_type(&self) -> AgentType {
        AgentType::Codex
    }

    /// 检查agent是否已经停止
    pub fn is_stopped(&self) -> bool {
        *self.stopped.blocking_lock()
    }

    /// 获取CancellationToken
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel_token
    }
}

/// Claude Code Agent停止句柄
#[derive(Clone)]
pub struct ClaudeCodeAgentStopHandle {
    /// 取消令牌
    cancel_token: CancellationToken,
    /// 子进程句柄（可能为虚拟进程）
    child_process: Arc<Mutex<Option<tokio::process::Child>>>,
    /// stderr读取任务
    stderr_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// 项目ID（用于日志）
    project_id: String,
    /// 是否已经停止
    stopped: Arc<Mutex<bool>>,
}

impl ClaudeCodeAgentStopHandle {
    /// 使用外部CancellationToken和stderr流创建新的Claude Code Agent停止句柄
    pub fn with_cancellation_token_and_stderr(
        child_process: tokio::process::Child,
        stderr: tokio::process::ChildStderr,
        project_id: String,
        cancel_token: CancellationToken,
    ) -> Self {
        // 创建stderr任务
        let cancel_token_for_stderr = cancel_token.clone();
        let stderr_task = tokio::task::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut stderr_reader = tokio::io::BufReader::new(stderr);
            let mut stderr_buffer = String::new();

            loop {
                // 检查取消令牌
                if cancel_token_for_stderr.is_cancelled() {
                    info!("Claude Code Agent stderr 任务收到取消信号，退出读取");
                    break;
                }

                match stderr_reader.read_line(&mut stderr_buffer).await {
                    Ok(0) => {
                        info!("Claude Code Agent stderr 流已关闭");
                        break;
                    }
                    Ok(bytes_read) => {
                        let line = &stderr_buffer[..bytes_read];
                        if !line.trim().is_empty() {
                            warn!("Claude Code Agent stderr: {}", line.trim());
                        }
                        stderr_buffer.clear();
                    }
                    Err(e) => {
                        error!("读取 Claude Code Agent stderr 失败: {}", e);
                        break;
                    }
                }
            }
        });

        Self {
            cancel_token,
            child_process: Arc::new(Mutex::new(Some(child_process))),
            stderr_task: Arc::new(Mutex::new(Some(stderr_task))),
            project_id,
            stopped: Arc::new(Mutex::new(false)),
        }
    }

    /// 取消agent服务
    pub fn cancel(&self) {
        if !self.cancel_token.is_cancelled() {
            info!("发送取消信号到Claude Code Agent[{}]", self.project_id);
            self.cancel_token.cancel();
        }
    }

    /// 停止agent服务
    pub async fn stop(&self) -> Result<()> {
        let mut stopped_guard = self.stopped.lock().await;
        if *stopped_guard {
            info!(
                "Claude Code Agent[{}]已经停止，无需重复停止",
                self.project_id
            );
            return Ok(());
        }

        // 先发送取消信号
        if !self.cancel_token.is_cancelled() {
            self.cancel_token.cancel();
        }

        info!("正在停止Claude Code Agent[{}]", self.project_id);

        // 1. 停止stderr任务
        if let Some(task) = self.stderr_task.lock().await.take() {
            debug!("停止Claude Code Agent[{}] stderr任务", self.project_id);
            task.abort();
            // 等待任务结束
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        // 2. 终止子进程 - 利用 kill_on_drop(true) 特性防止僵尸进程
        if let Some(mut child) = self.child_process.lock().await.take() {
            debug!(
                "Claude Code Agent[{}]子进程将通过 drop 自动终止，防止僵尸进程",
                self.project_id
            );

            // 使用 tokio::select! 来优雅地处理子进程终止
            let child_monitor = async {
                // 等待子进程自然退出或通过 drop 终止
                match child.wait().await {
                    Ok(status) => {
                        debug!(
                            "Claude Code Agent[{}]子进程退出状态: {:?}",
                            self.project_id, status
                        );
                    }
                    Err(e) => {
                        error!(
                            "等待Claude Code Agent[{}]子进程退出失败: {:?}",
                            self.project_id, e
                        );
                    }
                }
            };

            // 等待子进程退出，但有超时保护
            tokio::select! {
                _ = child_monitor => {
                    debug!("Claude Code Agent[{}]子进程已正常退出", self.project_id);
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(5)) => {
                    warn!("Claude Code Agent[{}]子进程退出超时，kill_on_drop(true) 会确保进程被终止并防止僵尸进程", self.project_id);
                }
            }

            // 注意：Child 对象在这里被 drop，kill_on_drop(true) 会自动杀死子进程
            // 这确保了不会有僵尸进程产生
        }

        *stopped_guard = true;
        info!("Claude Code Agent[{}]停止完成", self.project_id);
        Ok(())
    }

    /// 获取agent类型
    pub fn agent_type(&self) -> AgentType {
        AgentType::Claude
    }

    /// 检查agent是否已经停止
    pub fn is_stopped(&self) -> bool {
        *self.stopped.blocking_lock()
    }

    /// 获取CancellationToken
    pub fn cancellation_token(&self) -> &CancellationToken {
        &self.cancel_token
    }
}

/// Agent停止句柄的Arc包装器
pub type AgentStopHandleArc = Arc<AgentStopHandle>;

/// 绑定到 ProjectAndAgentInfo 生命周期的停止守卫
#[derive(Clone)]
pub struct AgentStopGuard {
    inner: AgentStopHandleArc,
    dropped: Arc<AtomicBool>,
}

impl AgentStopGuard {
    pub fn new(inner: AgentStopHandleArc) -> Self {
        Self {
            inner,
            dropped: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.inner.cancel();
    }

    pub fn agent_type(&self) -> AgentType {
        self.inner.agent_type()
    }
    pub fn is_stopped(&self) -> bool {
        self.inner.is_stopped()
    }
    pub fn cancellation_token(&self) -> CancellationToken {
        self.inner.cancellation_token()
    }

    /// 异步停止（克隆内部句柄以避免消耗 Guard 本身）
    pub async fn stop_async(&self) -> Result<()> {
        let owned = self.inner.as_ref().clone();
        owned.stop().await
    }
}

impl Drop for AgentStopGuard {
    fn drop(&mut self) {
        // 确保仅首次 drop 触发
        if self.dropped.swap(true, Ordering::SeqCst) {
            return;
        }

        // 发送取消信号（非阻塞）
        self.inner.cancel();

        // 若当前在线程中存在 Tokio 运行时，则后台停止
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let inner = self.inner.clone();
            handle.spawn(async move {
                let owned = inner.as_ref().clone();
                let _ = owned.stop().await; // 忽略错误，确保尽力而为
            });
        }
        // 若无运行时，则仅依赖 kill_on_drop(true) 在 Child drop 时兜底
    }
}
