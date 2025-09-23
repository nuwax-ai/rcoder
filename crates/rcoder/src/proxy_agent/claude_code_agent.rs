use std::path::PathBuf;
use std::process::Stdio;

use agent_client_protocol::{self as acp, Agent};
use anyhow::{Context, Result, anyhow};
use tokio::process::Command;
use tokio::task::LocalSet;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{debug, error, info};

/// Claude Code ACP 客户端实现
///
/// 实现了 acp::Client trait，用于处理来自代理的请求和通知
pub struct ClaudeCodeAcpClient {
    session_notifications: tokio::sync::mpsc::UnboundedSender<acp::SessionNotification>,
}

impl ClaudeCodeAcpClient {
    /// 创建新的客户端实例
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        Self {
            session_notifications: tx,
        }
    }

    /// 获取会话通知发送器
    pub fn session_notification_tx(
        &self,
    ) -> &tokio::sync::mpsc::UnboundedSender<acp::SessionNotification> {
        &self.session_notifications
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Client for ClaudeCodeAcpClient {
    async fn request_permission(
        &self,
        _args: acp::RequestPermissionRequest,
    ) -> Result<acp::RequestPermissionResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn write_text_file(
        &self,
        _args: acp::WriteTextFileRequest,
    ) -> Result<acp::WriteTextFileResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn read_text_file(
        &self,
        _args: acp::ReadTextFileRequest,
    ) -> Result<acp::ReadTextFileResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn create_terminal(
        &self,
        _args: acp::CreateTerminalRequest,
    ) -> Result<acp::CreateTerminalResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _args: acp::TerminalOutputRequest,
    ) -> Result<acp::TerminalOutputResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _args: acp::ReleaseTerminalRequest,
    ) -> Result<acp::ReleaseTerminalResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _args: acp::WaitForTerminalExitRequest,
    ) -> Result<acp::WaitForTerminalExitResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn kill_terminal_command(
        &self,
        _args: acp::KillTerminalCommandRequest,
    ) -> Result<acp::KillTerminalCommandResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> Result<(), acp::Error> {
        // 转发会话通知到通道
        let _ = self.session_notifications.send(args);
        Ok(())
    }

    async fn ext_method(&self, _args: acp::ExtRequest) -> Result<acp::ExtResponse, acp::Error> {
        Err(acp::Error::method_not_found())
    }

    async fn ext_notification(&self, _args: acp::ExtNotification) -> Result<(), acp::Error> {
        Err(acp::Error::method_not_found())
    }
}
