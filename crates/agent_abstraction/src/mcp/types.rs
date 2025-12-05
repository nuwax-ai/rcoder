//! MCP 模块的核心类型定义

use std::time::Instant;

use rmcp::model::{CallToolResult, ServerInfo};
use serde::{Deserialize, Serialize};

/// MCP 服务状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpServerStatus {
    /// 未启动
    Stopped,
    /// 启动中
    Starting,
    /// 运行中
    Running,
    /// 停止中
    Stopping,
    /// 错误状态
    Error,
}

impl Default for McpServerStatus {
    fn default() -> Self {
        Self::Stopped
    }
}

/// MCP 服务信息 (用于外部查询)
#[derive(Debug, Clone)]
pub struct McpServerInfo {
    /// 服务名称
    pub name: String,
    /// 服务状态
    pub status: McpServerStatus,
    /// 进程 ID (如果运行中)
    pub pid: Option<u32>,
    /// 启动时间
    pub started_at: Option<Instant>,
    /// 服务端信息 (MCP 协议返回)
    pub server_info: Option<ServerInfo>,
    /// 可用工具数量
    pub tool_count: Option<usize>,
}

impl McpServerInfo {
    /// 创建一个已停止状态的服务信息
    pub fn stopped(name: String) -> Self {
        Self {
            name,
            status: McpServerStatus::Stopped,
            pid: None,
            started_at: None,
            server_info: None,
            tool_count: None,
        }
    }

    /// 获取运行时长
    pub fn uptime(&self) -> Option<std::time::Duration> {
        self.started_at.map(|t| t.elapsed())
    }
}

/// 工具调用请求
#[derive(Debug, Clone)]
pub struct ToolCallRequest {
    /// 工具名称
    pub name: String,
    /// 调用参数 (JSON 对象)
    pub arguments: Option<serde_json::Map<String, serde_json::Value>>,
}

impl ToolCallRequest {
    /// 创建一个无参数的工具调用请求
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            arguments: None,
        }
    }

    /// 创建一个带参数的工具调用请求
    pub fn with_arguments(
        name: impl Into<String>,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        Self {
            name: name.into(),
            arguments: Some(arguments),
        }
    }
}

/// 工具调用结果
#[derive(Debug, Clone)]
pub struct ToolCallResponse {
    /// 是否成功
    pub success: bool,
    /// 结果内容
    pub content: Vec<String>,
    /// 错误信息
    pub error: Option<String>,
}

impl ToolCallResponse {
    /// 创建成功响应
    pub fn success(content: Vec<String>) -> Self {
        Self {
            success: true,
            content,
            error: None,
        }
    }

    /// 创建失败响应
    pub fn error(error: impl Into<String>) -> Self {
        Self {
            success: false,
            content: Vec::new(),
            error: Some(error.into()),
        }
    }
}

impl From<CallToolResult> for ToolCallResponse {
    fn from(result: CallToolResult) -> Self {
        let is_error = result.is_error.unwrap_or(false);
        let content: Vec<String> = result
            .content
            .iter()
            .filter_map(|c| {
                // 通过 Deref 访问 RawContent，然后提取文本内容
                if let Some(text) = c.as_text() {
                    Some(text.text.clone())
                } else {
                    None
                }
            })
            .collect();

        Self {
            success: !is_error,
            content,
            error: if is_error {
                Some("Tool returned error".to_string())
            } else {
                None
            },
        }
    }
}
