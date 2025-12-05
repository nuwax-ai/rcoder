//! Agent launcher implementations.
//!
//! 提供 Agent 进程启动和连接管理的抽象。
//!
//! # 状态流转
//!
//! ```text
//! ProcessLaunchInfo ──→ LaunchedProcess ──→ ConnectedAgent
//!                       (已启动，有 stdio)   (已连接，有 ACP)
//! ```

mod subprocess;

use std::sync::Arc;

use agent_client_protocol::{ClientSideConnection, SessionId};
use tokio_util::sync::CancellationToken;

pub use subprocess::{LaunchedProcess, SubprocessLauncher};

/// Agent process status
#[derive(Debug, Clone, PartialEq)]
pub enum ProcessStatus {
    /// Process is running
    Running,
    /// Process has exited
    Exited(i32),
    /// Process status is unknown
    Unknown,
}

/// Termination result
#[derive(Debug)]
pub struct TerminationResult {
    /// Exit code
    pub exit_code: Option<i32>,
    /// Whether termination was successful
    pub success: bool,
}

/// 已连接的 Agent
///
/// 表示已经建立 ACP 连接的 Agent，包含：
/// - 底层进程信息 (`LaunchedProcess`)
/// - ACP 客户端连接 (`ClientSideConnection`)
/// - 会话 ID（newSession 成功后分配）
#[derive(Debug)]
pub struct ConnectedAgent {
    /// 底层已启动的进程
    pub process: LaunchedProcess,
    /// ACP 客户端连接
    pub client_conn: Arc<ClientSideConnection>,
    /// Session ID (newSession 成功后分配)
    pub session_id: Option<SessionId>,
    /// Stderr 日志任务句柄
    pub stderr_task: Option<tokio::task::JoinHandle<()>>,
}

impl ConnectedAgent {
    /// 创建新的已连接 Agent
    pub fn new(
        process: LaunchedProcess,
        client_conn: Arc<ClientSideConnection>,
    ) -> Self {
        Self {
            process,
            client_conn,
            session_id: None,
            stderr_task: None,
        }
    }

    /// 设置 session ID
    pub fn set_session_id(&mut self, session_id: SessionId) {
        self.session_id = Some(session_id);
    }

    /// 设置 stderr 任务
    pub fn set_stderr_task(&mut self, task: tokio::task::JoinHandle<()>) {
        self.stderr_task = Some(task);
    }

    /// 获取进程 ID
    pub fn agent_id(&self) -> &str {
        self.process.process.id()
    }

    /// 获取取消令牌
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.process.cancel_token
    }

    /// 检查进程状态
    pub fn try_wait(&self) -> Result<Option<std::process::ExitStatus>, std::io::Error> {
        self.process.process.try_wait()
    }

    /// 终止进程
    pub async fn kill(&self) -> Result<(), std::io::Error> {
        self.process.cancel_token.cancel();
        self.process.process.kill().await
    }
}

/// Agent launcher trait
///
/// 负责启动 Agent 进程，返回 `LaunchedProcess`。
/// 调用方负责使用 stdio 句柄建立 ACP 连接。
#[async_trait::async_trait(?Send)]
pub trait AgentLauncher {
    /// 启动 Agent 进程
    ///
    /// 返回 `LaunchedProcess`，包含 stdin/stdout/stderr 句柄。
    /// 调用方需要使用这些句柄建立 ACP 连接，创建 `ConnectedAgent`。
    async fn launch(
        &self,
        info: crate::traits::agent::ProcessLaunchInfo,
    ) -> Result<LaunchedProcess, Box<dyn std::error::Error + Send + Sync>>;

    /// 终止已启动的进程
    async fn terminate(
        &self,
        launched: &LaunchedProcess,
    ) -> Result<TerminationResult, Box<dyn std::error::Error + Send + Sync>> {
        launched.cancel_token.cancel();
        match launched.process.kill().await {
            Ok(_) => Ok(TerminationResult {
                exit_code: None,
                success: true,
            }),
            Err(_) => Ok(TerminationResult {
                exit_code: None,
                success: false,
            }),
        }
    }

    /// 检查进程状态
    async fn check_status(
        &self,
        launched: &LaunchedProcess,
    ) -> Result<ProcessStatus, Box<dyn std::error::Error + Send + Sync>> {
        match launched.process.try_wait() {
            Ok(Some(status)) => {
                let code = status.code().unwrap_or(-1);
                Ok(ProcessStatus::Exited(code))
            }
            Ok(None) => Ok(ProcessStatus::Running),
            Err(_) => Ok(ProcessStatus::Unknown),
        }
    }
}

// ============================================================================
// 向后兼容：保留 LaunchedAgent 作为 ConnectedAgent 的别名
// ============================================================================

/// 向后兼容的类型别名
#[deprecated(since = "0.2.0", note = "Use ConnectedAgent instead")]
pub type LaunchedAgent = ConnectedAgent;
