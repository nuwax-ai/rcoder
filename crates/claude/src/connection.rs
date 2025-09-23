//! Claude Code ACP 客户端连接模块
//!
//! 提供自动检查安装 claude-code-acp 工具并通过 ClientSideConnection 连接到子进程启动的 ACP 代理服务。

use std::path::PathBuf;
use std::process::Stdio;

use agent_client_protocol::{self as acp, Agent};
use anyhow::{anyhow, Context, Result};
use tokio::process::Command;
use tokio::task::LocalSet;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::{debug, error, info};

use crate::util::{
    ClaudeCodeAcpConfig, ClaudeCodeAcpManager,
};

/// Claude Code ACP 客户端连接器
///
/// 负责自动检查安装 claude-code-acp 工具，并通过 ClientSideConnection 连接到子进程启动的 ACP 代理服务
pub struct ClaudeCodeAcpConnector {
    manager: ClaudeCodeAcpManager,
}

impl ClaudeCodeAcpConnector {
    /// 创建新的连接器实例
    pub fn new(config: ClaudeCodeAcpConfig) -> Self {
        Self {
            manager: ClaudeCodeAcpManager::new(config),
        }
    }

    /// 使用默认配置创建连接器
    pub fn default() -> Self {
        Self::new(ClaudeCodeAcpConfig::default())
    }

    /// 自动检查并安装 claude-code-acp，然后建立连接
    pub async fn connect(&self) -> Result<ClaudeCodeAcpConnection> {
        info!("开始检查和安装 claude-code-acp");

        // 检查安装状态
        let status = self.manager.get_status().await?;
        debug!("当前安装状态: {:?}", status);

        // 确保已安装
        if !self.manager.is_available().await {
            info!("Claude Code ACP 未安装，正在安装...");
            self.manager.install_or_update().await?;
        }
        let command = self.manager.get_command().await?;
        info!("claude-code-acp 已安装或验证通过");

        // 启动子进程
        info!("启动 claude-code-acp 子进程: {:?}", command);
        let mut child = Command::new(&command.path)
            .args(&command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .envs(command.env.unwrap_or_default())
            .spawn()
            .context("无法启动 claude-code-acp 子进程")?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("无法获取子进程 stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("无法获取子进程 stdout"))?;

        // 创建兼容的流
        let outgoing = stdin.compat_write();
        let incoming = stdout.compat();

        // 创建客户端
        let client = ClaudeCodeAcpClient::new();

        // 创建 LocalSet 用于运行非 Send 的 future
        let local_set = LocalSet::new();

        // 创建连接
        let (connection, handle_io) = acp::ClientSideConnection::new(
            client,
            outgoing,
            incoming,
            |fut| {
                tokio::task::spawn_local(fut);
            },
        );

        // 启动 I/O 处理任务
        tokio::task::spawn_local(handle_io);

        Ok(ClaudeCodeAcpConnection {
            connection,
            child: Some(child),
            local_set,
        })
    }

    /// 获取当前配置
    pub fn config(&self) -> ClaudeCodeAcpConfig {
        // TODO: 需要在 ClaudeCodeAcpManager 中添加 config() 方法
        // 暂时返回默认配置
        ClaudeCodeAcpConfig::default()
    }

    /// 更新配置
    pub fn with_config(mut self, config: ClaudeCodeAcpConfig) -> Self {
        self.manager = ClaudeCodeAcpManager::new(config);
        self
    }
}

/// Claude Code ACP 连接
///
/// 封装了 ClientSideConnection 和相关资源，提供简洁的接口
pub struct ClaudeCodeAcpConnection {
    connection: acp::ClientSideConnection,
    child: Option<tokio::process::Child>,
    local_set: LocalSet,
}

impl ClaudeCodeAcpConnection {
    /// 初始化连接
    pub async fn initialize(&mut self) -> Result<()> {
        self.local_set
            .run_until(async {
                self.connection
                    .initialize(acp::InitializeRequest {
                        protocol_version: acp::V1,
                        client_capabilities: acp::ClientCapabilities::default(),
                        meta: None,
                    })
                    .await
                    .context("初始化 ACP 连接失败")?;
                Ok::<(), anyhow::Error>(())
            })
            .await
    }

    /// 创建新会话
    pub async fn new_session(&mut self, cwd: Option<PathBuf>) -> Result<acp::NewSessionResponse> {
        let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        self.local_set
            .run_until(async {
                let response = self.connection
                    .new_session(acp::NewSessionRequest {
                        mcp_servers: Vec::new(),
                        cwd,
                        meta: None,
                    })
                    .await
                    .context("创建 ACP 会话失败")?;
                Ok::<acp::NewSessionResponse, anyhow::Error>(response)
            })
            .await
    }

    /// 发送提示
    pub async fn prompt(&mut self, session_id: acp::SessionId, prompt: Vec<String>) -> Result<acp::PromptResponse> {
        self.local_set
            .run_until(async {
                let response = self.connection
                    .prompt(acp::PromptRequest {
                        session_id,
                        prompt: prompt.into_iter().map(|s| s.into()).collect(),
                        meta: None,
                    })
                    .await
                    .context("发送提示失败")?;
                Ok::<acp::PromptResponse, anyhow::Error>(response)
            })
            .await
    }

    /// 获取会话通知接收器
    pub fn subscribe(&self) -> acp::StreamReceiver {
        self.connection.subscribe()
    }

    /// 关闭连接并清理资源
    pub async fn close(mut self) -> Result<()> {
        // 停止子进程
        if let Some(mut child) = self.child.take() {
            if let Err(e) = child.kill().await {
                error!("停止子进程失败: {}", e);
            }
            if let Err(e) = child.wait().await {
                error!("等待子进程退出失败: {}", e);
            }
        }

        Ok(())
    }
}

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
    pub fn session_notification_tx(&self) -> &tokio::sync::mpsc::UnboundedSender<acp::SessionNotification> {
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

    async fn session_notification(
        &self,
        args: acp::SessionNotification,
    ) -> Result<(), acp::Error> {
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

/// 连接管理器
///
/// 提供更高层次的连接管理功能
pub struct ClaudeCodeAcpConnectionManager {
    connector: ClaudeCodeAcpConnector,
}

impl ClaudeCodeAcpConnectionManager {
    /// 创建新的连接管理器
    pub fn new(config: ClaudeCodeAcpConfig) -> Self {
        Self {
            connector: ClaudeCodeAcpConnector::new(config),
        }
    }

    /// 使用默认配置创建连接管理器
    pub fn default() -> Self {
        Self::new(ClaudeCodeAcpConfig::default())
    }

    /// 创建并初始化连接
    pub async fn create_connection(&self) -> Result<ClaudeCodeAcpConnection> {
        let mut connection = self.connector.connect().await?;
        connection.initialize().await?;
        Ok(connection)
    }

    /// 创建连接并启动会话
    pub async fn create_connection_with_session(
        &self,
        cwd: Option<PathBuf>,
    ) -> Result<(ClaudeCodeAcpConnection, acp::NewSessionResponse)> {
        let mut connection = self.create_connection().await?;
        let session_response = connection.new_session(cwd).await?;
        Ok((connection, session_response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_connector_creation() {
        let config = ClaudeCodeAcpConfig::default();
        let connector = ClaudeCodeAcpConnector::new(config);
        assert_eq!(connector.config().package_name, "@zed-industries/claude-code-acp");
    }

    #[tokio::test]
    async fn test_client_creation() {
        let client = ClaudeCodeAcpClient::new();
        // 测试客户端创建
        assert!(client.session_notification_tx().is_closed());
    }

    #[tokio::test]
    async fn test_connection_manager_creation() {
        let manager = ClaudeCodeAcpConnectionManager::default();
        // 测试连接管理器创建
        assert_eq!(manager.connector.config().package_name, "@zed-industries/claude-code-acp");
    }
}