//! MCP server instance wrapper using rmcp
//!
//! 使用 rmcp 官方库实现 MCP 服务实例的管理。

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_config::McpServerConfig;
use rmcp::{
    model::{CallToolRequestParam, Tool},
    service::{RunningService, ServiceExt},
    transport::TokioChildProcess,
    RoleClient,
};
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::error::{McpError, McpResult};
use super::types::{McpServerInfo, McpServerStatus, ToolCallRequest, ToolCallResponse};

/// MCP 服务句柄 (内部状态)
struct McpServerHandle {
    /// rmcp 运行中的服务
    service: RunningService<RoleClient, ()>,
    /// 进程 ID
    pid: Option<u32>,
    /// 缓存的工具列表
    tools: Vec<Tool>,
}

/// MCP server instance
///
/// 封装单个 MCP 服务的生命周期管理，包括：
/// - 启动/停止服务进程
/// - 查询服务状态
/// - 列出可用工具
/// - 调用工具
pub struct McpServerInstance {
    /// Server name
    name: String,

    /// Server configuration
    config: McpServerConfig,

    /// 内部句柄 (运行时状态)
    handle: RwLock<Option<McpServerHandle>>,

    /// 启动时间
    started_at: RwLock<Option<Instant>>,

    /// 取消令牌
    cancellation_token: CancellationToken,
}

impl McpServerInstance {
    /// Create a new MCP server instance
    pub fn new(name: String, config: McpServerConfig) -> Self {
        Self {
            name,
            config,
            handle: RwLock::new(None),
            started_at: RwLock::new(None),
            cancellation_token: CancellationToken::new(),
        }
    }

    /// Start the MCP server
    pub async fn start(&self) -> McpResult<()> {
        // 检查是否已运行
        {
            let handle = self.handle.read().await;
            if handle.is_some() {
                return Err(McpError::AlreadyRunning(self.name.clone()));
            }
        }

        // 获取命令配置
        let command = self
            .config
            .command
            .as_ref()
            .ok_or_else(|| McpError::ConfigError("MCP server command is required".into()))?;

        // 构建 tokio Command
        let mut cmd = Command::new(command);

        // 添加参数
        if let Some(ref args) = self.config.args {
            cmd.args(args);
        }

        // 添加环境变量
        if let Some(ref env) = self.config.env {
            for (key, value) in env.iter() {
                cmd.env(key, value);
            }
        }

        // 创建 TokioChildProcess
        let child_process = TokioChildProcess::new(cmd)
            .map_err(|e| McpError::StartFailed(format!("Failed to spawn process: {}", e)))?;

        // 获取 PID
        let pid = child_process.id();

        // 使用 rmcp 建立连接
        let timeout = self.config.timeout.unwrap_or(Duration::from_secs(30));

        let service = tokio::time::timeout(timeout, async {
            ()
                .serve_with_ct(child_process, self.cancellation_token.child_token())
                .await
        })
        .await
        .map_err(|_| McpError::Timeout("MCP server initialization timed out".into()))?
        .map_err(|e| McpError::InitializeFailed(format!("{:?}", e)))?;

        // 获取工具列表
        let tools_result = service.list_all_tools().await;
        let tools = tools_result.unwrap_or_default();

        tracing::info!(
            name = %self.name,
            pid = ?pid,
            tool_count = tools.len(),
            "MCP server started successfully"
        );

        // 更新状态
        {
            let mut handle_guard = self.handle.write().await;
            *handle_guard = Some(McpServerHandle { service, pid, tools });
        }

        {
            let mut started_at = self.started_at.write().await;
            *started_at = Some(Instant::now());
        }

        Ok(())
    }

    /// Stop the MCP server
    pub async fn stop(&self) -> McpResult<()> {
        let handle = {
            let mut handle_guard = self.handle.write().await;
            handle_guard.take()
        };

        if let Some(handle) = handle {
            // 使用 cancel 优雅关闭
            if let Err(e) = handle.service.cancel().await {
                tracing::warn!(name = %self.name, error = ?e, "Error during MCP server shutdown");
            }

            tracing::info!(name = %self.name, "MCP server stopped");
        }

        // 清理状态
        {
            let mut started_at = self.started_at.write().await;
            *started_at = None;
        }

        Ok(())
    }

    /// Check if server is running
    pub async fn is_running(&self) -> bool {
        let handle = self.handle.read().await;
        handle.is_some()
    }

    /// Get server status
    pub async fn status(&self) -> McpServerStatus {
        if self.is_running().await {
            McpServerStatus::Running
        } else {
            McpServerStatus::Stopped
        }
    }

    /// Get server info
    pub async fn info(&self) -> McpServerInfo {
        let handle = self.handle.read().await;
        let started_at = self.started_at.read().await;

        McpServerInfo {
            name: self.name.clone(),
            status: if handle.is_some() {
                McpServerStatus::Running
            } else {
                McpServerStatus::Stopped
            },
            pid: handle.as_ref().and_then(|h| h.pid),
            started_at: *started_at,
            server_info: handle.as_ref().and_then(|h| h.service.peer_info().cloned()),
            tool_count: handle.as_ref().map(|h| h.tools.len()),
        }
    }

    /// Get uptime
    pub async fn uptime(&self) -> Option<Duration> {
        let started_at = self.started_at.read().await;
        started_at.map(|t| t.elapsed())
    }

    /// List available tools
    pub async fn list_tools(&self) -> McpResult<Vec<Tool>> {
        let handle = self.handle.read().await;
        let handle = handle
            .as_ref()
            .ok_or_else(|| McpError::NotRunning(self.name.clone()))?;

        Ok(handle.tools.clone())
    }

    /// Refresh tools list from server
    pub async fn refresh_tools(&self) -> McpResult<Vec<Tool>> {
        let mut handle_guard = self.handle.write().await;
        let handle = handle_guard
            .as_mut()
            .ok_or_else(|| McpError::NotRunning(self.name.clone()))?;

        let tools = handle
            .service
            .list_all_tools()
            .await
            .map_err(|e| McpError::ToolCallFailed(format!("Failed to refresh tools: {}", e)))?;

        handle.tools = tools.clone();
        Ok(tools)
    }

    /// Call a tool
    pub async fn call_tool(&self, request: ToolCallRequest) -> McpResult<ToolCallResponse> {
        let handle = self.handle.read().await;
        let handle = handle
            .as_ref()
            .ok_or_else(|| McpError::NotRunning(self.name.clone()))?;

        // 检查工具是否存在
        if !handle.tools.iter().any(|t| t.name == request.name) {
            return Err(McpError::ToolNotFound(request.name));
        }

        let result = handle
            .service
            .call_tool(CallToolRequestParam {
                name: request.name.into(),
                arguments: request.arguments,
            })
            .await
            .map_err(|e| McpError::ToolCallFailed(format!("{}", e)))?;

        Ok(ToolCallResponse::from(result))
    }

    /// Get server name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get server config
    pub fn config(&self) -> &McpServerConfig {
        &self.config
    }
}

impl Drop for McpServerInstance {
    fn drop(&mut self) {
        // 触发取消令牌，通知服务关闭
        self.cancellation_token.cancel();
    }
}

impl std::fmt::Debug for McpServerInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerInstance")
            .field("name", &self.name)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}
