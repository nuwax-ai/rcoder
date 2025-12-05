//! Subprocess-based agent launcher.

use async_trait::async_trait;
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::{AgentLauncher, ProcessStatus, TerminationResult};
use crate::process::AgentProcess;

/// Launched process with stdio handles
///
/// 表示已启动但尚未建立 ACP 连接的进程。
/// 调用方需要使用 stdin/stdout 建立 ACP 连接，然后创建 `ConnectedAgent`。
///
/// # 状态流转
/// ```text
/// SubprocessLauncher::launch() -> LaunchedProcess -> ConnectedAgent
/// ```
#[derive(Debug)]
pub struct LaunchedProcess {
    /// Agent process wrapper
    pub process: AgentProcess,
    /// Stdin handle for ACP communication
    pub stdin: ChildStdin,
    /// Stdout handle for ACP communication
    pub stdout: ChildStdout,
    /// Stderr handle for logging
    pub stderr: ChildStderr,
    /// Cancellation token for lifecycle management
    pub cancel_token: CancellationToken,
}

impl LaunchedProcess {
    /// 获取进程 ID
    pub fn agent_id(&self) -> &str {
        self.process.id()
    }

    /// 获取配置
    pub fn config(&self) -> &agent_config::AgentConfig {
        self.process.config()
    }
}

/// Subprocess-based agent launcher
///
/// 负责启动 Agent 子进程，返回 stdio 句柄。
/// 不负责建立 ACP 连接 - 这是调用方的责任。
#[derive(Debug, Default)]
pub struct SubprocessLauncher;

impl SubprocessLauncher {
    /// Create a new subprocess launcher
    pub fn new() -> Self {
        Self
    }
}

#[async_trait(?Send)]
impl AgentLauncher for SubprocessLauncher {
    /// 启动 Agent 进程
    ///
    /// 返回 `LaunchedProcess`，包含 stdin/stdout/stderr 句柄。
    /// 调用方需要：
    /// 1. 使用 stdin/stdout 建立 ACP 连接
    /// 2. 创建 `ConnectedAgent`
    async fn launch(
        &self,
        info: crate::traits::agent::ProcessLaunchInfo,
    ) -> Result<LaunchedProcess, Box<dyn std::error::Error + Send + Sync>> {
        info!(
            "启动 Agent 进程: {} {:?}, 工作目录: {}",
            info.command,
            info.args,
            info.working_dir.display()
        );

        // Build command
        let mut cmd = tokio::process::Command::new(&info.command);
        cmd.args(&info.args)
            .envs(&info.env)
            .current_dir(&info.working_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // Spawn the process
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("无法启动 Agent 进程 '{}': {}", info.command, e))?;

        let pid = child.id().unwrap_or(0);
        info!("Agent 进程已启动, PID: {}", pid);

        // Take stdio handles
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "无法获取进程 stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "无法获取进程 stdout".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "无法获取进程 stderr".to_string())?;

        // Create process wrapper
        let process = AgentProcess::new(info.id.clone(), child, info.config);

        // Create cancellation token
        let cancel_token = CancellationToken::new();

        debug!("Agent 进程 stdio 句柄已获取, 等待建立 ACP 连接");

        Ok(LaunchedProcess {
            process,
            stdin,
            stdout,
            stderr,
            cancel_token,
        })
    }

    async fn terminate(
        &self,
        launched: &LaunchedProcess,
    ) -> Result<TerminationResult, Box<dyn std::error::Error + Send + Sync>> {
        info!("终止 Agent 进程: {}", launched.process.id());

        // Cancel the token to signal shutdown
        launched.cancel_token.cancel();

        // Try to kill the process
        match launched.process.kill().await {
            Ok(_) => {
                info!("Agent 进程已终止: {}", launched.process.id());
                Ok(TerminationResult {
                    exit_code: None,
                    success: true,
                })
            }
            Err(e) => {
                tracing::warn!("终止 Agent 进程失败: {}", e);
                Ok(TerminationResult {
                    exit_code: None,
                    success: false,
                })
            }
        }
    }

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
