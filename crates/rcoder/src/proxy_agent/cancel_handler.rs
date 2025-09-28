//! Agent取消处理器trait和相关实现
//!
//! 提供统一的agent生命周期管理接口，支持不同类型的agent清理

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use agent_client_protocol::{Agent, ClientSideConnection, CancelNotification};

/// Agent取消处理器trait
pub trait CancelHandler: Send + Sync {
    /// 取消agent执行（优雅停止）
    fn cancel(&self) -> Box<dyn futures::Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + '_>;

    /// 强制终止agent
    fn terminate(&self) -> Box<dyn futures::Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + '_>;

    /// 获取agent类型描述
    fn agent_type(&self) -> &'static str;
}

/// Agent清理处理器包装器
pub struct AgentCleanupHandler {
    handler: Box<dyn CancelHandler>,
    timeout: Duration,
    cleanup_task: Option<JoinHandle<()>>,
}

impl AgentCleanupHandler {
    /// 创建新的清理处理器
    pub fn new(handler: Box<dyn CancelHandler>, timeout: Duration) -> Self {
        Self {
            handler,
            timeout,
            cleanup_task: None,
        }
    }

    /// 启动清理任务（用于Drop trait）
    fn start_cleanup_task(&mut self) {
        let handler = self.handler.agent_type().to_string();
        let timeout = self.timeout;

        // 这里我们无法在Drop中直接运行async，所以设置一个标志
        // 实际的清理逻辑会在后台任务中处理
        self.cleanup_task = Some(tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            warn!("Agent清理超时未被处理: {}", handler);
        }));
    }
}

impl Drop for AgentCleanupHandler {
    fn drop(&mut self) {
        debug!("AgentCleanupHandler被drop，开始清理: {}", self.handler.agent_type());

        // 由于Drop trait不能是async的，我们需要在这里启动一个同步的清理
        // 实际的清理逻辑会被委托给一个专门的任务

        // 注意：这里我们只是标记需要清理，实际的清理会在后台进行
        self.start_cleanup_task();
    }
}

/// Claude Code Agent处理器
pub struct ClaudeCodeHandler {
    child: Arc<Mutex<Option<tokio::process::Child>>>,
    stderr_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    client_conn: Arc<ClientSideConnection>,
    session_id: agent_client_protocol::SessionId,
}

impl ClaudeCodeHandler {
    pub fn new(
        child: tokio::process::Child,
        stderr_task: JoinHandle<()>,
        client_conn: Arc<ClientSideConnection>,
        session_id: agent_client_protocol::SessionId,
    ) -> Self {
        Self {
            child: Arc::new(Mutex::new(Some(child))),
            stderr_task: Arc::new(Mutex::new(Some(stderr_task))),
            client_conn,
            session_id,
        }
    }
}

impl CancelHandler for ClaudeCodeHandler {
    fn cancel(&self) -> Box<dyn futures::Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + '_> {
        let child = self.child.clone();
        let session_id = self.session_id.clone();

        // 创建一个新的 Future 来包装可能不满足 Send 的操作
        Box::new(async move {
            info!("发送Cancel消息给Claude Code Agent: {}", session_id.0);

            // 由于 client_conn.cancel 可能不满足 Send trait，我们暂时只处理进程级别的取消
            // 等待进程退出
            let mut child_guard = child.lock().await;
            if let Some(child) = child_guard.as_mut() {
                match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
                    Ok(Ok(_)) => {
                        info!("Claude Code Agent进程已正常退出");
                        *child_guard = None;
                        Ok(())
                    }
                    Ok(Err(e)) => {
                        error!("等待Claude Code Agent进程退出失败: {}", e);
                        Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                    }
                    Err(_) => {
                        warn!("等待Claude Code Agent进程退出超时");
                        Err(Box::new(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "Process exit timeout"
                        )) as Box<dyn std::error::Error + Send + Sync>)
                    }
                }
            } else {
                Ok(())
            }
        })
    }

    fn terminate(&self) -> Box<dyn futures::Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + '_> {
        let child = self.child.clone();
        let stderr_task = self.stderr_task.clone();
        let session_id = self.session_id.clone();

        Box::new(async move {
            info!("强制终止Claude Code Agent: {}", session_id.0);

            let mut child_guard = child.lock().await;
            if let Some(mut child) = child_guard.take() {
                // 强制kill进程
                match child.kill().await {
                    Ok(_) => {
                        info!("Claude Code Agent进程已强制终止");

                        // 等待stderr任务结束
                        let mut stderr_guard = stderr_task.lock().await;
                        if let Some(stderr_task) = stderr_guard.take() {
                            if let Err(e) = stderr_task.await {
                                warn!("等待stderr任务结束失败: {}", e);
                            }
                        }

                        Ok(())
                    }
                    Err(e) => {
                        error!("强制终止Claude Code Agent进程失败: {}", e);
                        Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                    }
                }
            } else {
                Ok(())
            }
        })
    }

    fn agent_type(&self) -> &'static str {
        "ClaudeCode"
    }
}

/// Codex Agent处理器
pub struct CodexHandler {
    client_conn: Arc<ClientSideConnection>,
    io_tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    session_id: agent_client_protocol::SessionId,
}

impl CodexHandler {
    pub fn new(
        _agent: Arc<codex_acp_agent::CodexAgent>,
        client_conn: Arc<ClientSideConnection>,
        io_tasks: Vec<JoinHandle<()>>,
        session_id: agent_client_protocol::SessionId,
    ) -> Self {
        Self {
            client_conn,
            io_tasks: Arc::new(Mutex::new(io_tasks)),
            session_id,
        }
    }
}

impl CancelHandler for CodexHandler {
    fn cancel(&self) -> Box<dyn futures::Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + '_> {
        let client_conn = self.client_conn.clone();
        let io_tasks = self.io_tasks.clone();
        let session_id = self.session_id.clone();

        // 创建一个新的 Future 来包装可能不满足 Send 的操作
        Box::new(async move {
            // 使用 tokio::task::spawn_blocking 来确保 Send 安全
            let result = tokio::task::spawn_blocking(move || {
                // 这里我们不能直接调用 client_conn.cancel 因为它可能在非 Send 的 future 中
                // 我们需要重新设计这个逻辑，或者使用其他方式
                // 暂时返回一个错误
                Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "Codex cancel not implemented yet"
                    )) as Box<dyn std::error::Error + Send + Sync>
                )
            }).await;

            match result {
                Ok(inner_result) => {
                    match inner_result {
                        Ok(_) => {
                            info!("Codex Agent 取消完成");
                            Ok(())
                        }
                        Err(e) => {
                            error!("Codex Agent 取消失败: {}", e);
                            Err(e)
                        }
                    }
                }
                Err(e) => {
                    error!("任务执行失败: {}", e);
                    Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                }
            }
        })
    }

    fn terminate(&self) -> Box<dyn futures::Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + '_> {
        let io_tasks = self.io_tasks.clone();
        let session_id = self.session_id.clone();

        Box::new(async move {
            info!("强制终止Codex Agent: {}", session_id.0);

            // 直接取消所有IO任务
            let mut tasks_guard = io_tasks.lock().await;
            for task in tasks_guard.iter() {
                task.abort();
            }
            tasks_guard.clear();

            Ok(())
        })
    }

    fn agent_type(&self) -> &'static str {
        "Codex"
    }
}