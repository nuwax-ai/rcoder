use crate::permission::PermissionManager;
use crate::handle::{ResourceManager, TerminalHandle, FileOperationHandle};
use crate::types::ToolCallId;
use agent_client_protocol::{PermissionOption, PermissionOptionId};
use anyhow::Result;
use async_trait::async_trait;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::oneshot;

/// 基础Agent连接trait - 定义核心必需功能
#[async_trait]
pub trait AgentConnection: Send + Sync {
    /// 核心必需方法：执行工具调用
    async fn execute_tool_call(&self, tool_call: ToolCall) -> Result<ToolCallResult>;
    
    /// 核心必需方法：取消工具调用
    async fn cancel_tool_call(&self, tool_call_id: &ToolCallId) -> Result<()>;
    
    /// 可选能力：权限管理
    fn permission_manager(&self) -> Option<Arc<dyn PermissionCapability>> {
        None
    }
    
    /// 可选能力：资源管理
    fn resource_manager(&self) -> Option<Arc<dyn ResourceCapability>> {
        None
    }
    
    /// 可选能力：进度报告
    fn progress_reporter(&self) -> Option<Arc<dyn ProgressCapability>> {
        None
    }
    
    /// 可选能力：环境管理
    fn environment_manager(&self) -> Option<Arc<dyn EnvironmentCapability>> {
        None
    }
    
    /// 可选能力：模型选择
    fn model_selector(&self) -> Option<Arc<dyn ModelSelectionCapability>> {
        None
    }
    
    /// 可选能力：会话管理
    fn session_manager(&self) -> Option<Arc<dyn SessionCapability>> {
        None
    }
}

/// 权限管理能力
#[async_trait]
pub trait PermissionCapability: Send + Sync {
    /// 请求工具调用权限
    async fn request_permission(
        &self, 
        tool_call_id: ToolCallId,
        options: Vec<PermissionOption>,
        timeout_seconds: Option<u64>,
    ) -> Result<PermissionOptionId>;
    
    /// 检查是否有权限执行特定工具
    async fn check_permission(&self, tool_name: &str) -> Result<bool>;
    
    /// 设置自动权限规则
    async fn set_auto_permission_rule(&self, tool_name: &str, always_allow: bool) -> Result<()>;
}

/// 资源管理能力
#[async_trait]
pub trait ResourceCapability: Send + Sync {
    /// 创建Terminal资源
    async fn create_terminal(
        &self,
        command: String,
        working_dir: Option<std::path::PathBuf>,
        output_limit: Option<usize>,
    ) -> Result<TerminalHandle>;
    
    /// 创建文件操作资源
    async fn create_file_operation(
        &self,
        operation: crate::handle::FileOperationType,
        path: std::path::PathBuf,
    ) -> Result<FileOperationHandle>;
    
    /// 清理已完成的资源
    async fn cleanup_resources(&self) -> Result<()>;
    
    /// 列出活跃资源
    async fn list_active_resources(&self) -> Result<Vec<String>>;
}

/// 进度报告能力
#[async_trait]
pub trait ProgressCapability: Send + Sync {
    /// 发送进度更新
    async fn report_progress(&self, tool_call_id: &ToolCallId, progress: ProgressUpdate) -> Result<()>;
    
    /// 订阅进度事件
    async fn subscribe_progress(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<ProgressEvent>>;
}

/// 环境管理能力
#[async_trait]
pub trait EnvironmentCapability: Send + Sync {
    /// 获取当前工作目录
    fn current_directory(&self) -> &std::path::Path;
    
    /// 设置工作目录
    async fn set_working_directory(&self, path: std::path::PathBuf) -> Result<()>;
    
    /// 获取环境变量
    fn get_environment_variable(&self, name: &str) -> Option<String>;
    
    /// 设置环境变量
    async fn set_environment_variable(&self, name: String, value: String) -> Result<()>;
    
    /// 获取Shell类型
    fn shell_type(&self) -> ShellType;
}

/// 模型选择能力
#[async_trait]
pub trait ModelSelectionCapability: Send + Sync {
    /// 列出可用模型
    async fn list_models(&self) -> Result<Vec<ModelInfo>>;
    
    /// 选择模型
    async fn select_model(&self, model_id: &str) -> Result<()>;
    
    /// 获取当前模型
    async fn current_model(&self) -> Result<ModelInfo>;
}

/// 会话管理能力
#[async_trait]
pub trait SessionCapability: Send + Sync {
    /// 创建新会话
    async fn create_session(&self, config: SessionConfig) -> Result<SessionId>;
    
    /// 恢复会话
    async fn resume_session(&self, session_id: &SessionId) -> Result<SessionInfo>;
    
    /// 结束会话
    async fn end_session(&self, session_id: &SessionId) -> Result<()>;
    
    /// 列出活跃会话
    async fn list_sessions(&self) -> Result<Vec<SessionInfo>>;
}

// 数据结构定义

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub tool_name: String,
    pub parameters: serde_json::Value,
    pub requires_permission: bool,
}

#[derive(Debug, Clone)]
pub struct ToolCallResult {
    pub tool_call_id: ToolCallId,
    pub success: bool,
    pub output: Option<String>,
    pub error: Option<String>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    pub message: String,
    pub percentage: Option<f32>,
    pub stage: String,
}

#[derive(Debug, Clone)]
pub struct ProgressEvent {
    pub tool_call_id: ToolCallId,
    pub update: ProgressUpdate,
    pub timestamp: std::time::Instant,
}

#[derive(Debug, Clone)]
pub enum ShellType {
    Bash,
    Zsh,
    PowerShell,
    Cmd,
    Fish,
    Other(String),
}

impl ShellType {
    pub fn from_env() -> Self {
        if let Ok(shell) = std::env::var("SHELL") {
            if shell.contains("bash") {
                Self::Bash
            } else if shell.contains("zsh") {
                Self::Zsh
            } else if shell.contains("fish") {
                Self::Fish
            } else {
                Self::Other(shell)
            }
        } else if cfg!(windows) {
            Self::PowerShell
        } else {
            Self::Bash // 默认
        }
    }
    
    pub fn command_prefix(&self) -> &str {
        match self {
            Self::Bash | Self::Zsh | Self::Fish => "bash -c",
            Self::PowerShell => "pwsh -Command",
            Self::Cmd => "cmd /C",
            Self::Other(_) => "sh -c",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub capabilities: Vec<ModelCapability>,
}

#[derive(Debug, Clone)]
pub enum ModelCapability {
    TextGeneration,
    CodeGeneration,
    ImageAnalysis,
    FunctionCalling,
    Streaming,
}

#[derive(Debug, Clone)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub model_id: Option<String>,
    pub system_prompt: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: SessionId,
    pub created_at: std::time::Instant,
    pub model_info: Option<ModelInfo>,
    pub message_count: usize,
    pub is_active: bool,
}

// 能力检查辅助函数

/// 检查Agent是否支持权限管理
pub fn has_permission_capability(agent: &dyn AgentConnection) -> bool {
    agent.permission_manager().is_some()
}

/// 检查Agent是否支持资源管理  
pub fn has_resource_capability(agent: &dyn AgentConnection) -> bool {
    agent.resource_manager().is_some()
}

/// 检查Agent是否支持进度报告
pub fn has_progress_capability(agent: &dyn AgentConnection) -> bool {
    agent.progress_reporter().is_some()
}

/// 检查Agent是否支持环境管理
pub fn has_environment_capability(agent: &dyn AgentConnection) -> bool {
    agent.environment_manager().is_some()
}

/// 检查Agent是否支持模型选择
pub fn has_model_selection_capability(agent: &dyn AgentConnection) -> bool {
    agent.model_selector().is_some()
}

/// 检查Agent是否支持会话管理
pub fn has_session_capability(agent: &dyn AgentConnection) -> bool {
    agent.session_manager().is_some()
}

// 默认实现示例

/// 基础Agent连接实现 - 只提供核心功能
pub struct BasicAgentConnection {
    id: String,
}

impl BasicAgentConnection {
    pub fn new() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

#[async_trait]
impl AgentConnection for BasicAgentConnection {
    async fn execute_tool_call(&self, tool_call: ToolCall) -> Result<ToolCallResult> {
        // 基础实现 - 只是示例
        Ok(ToolCallResult {
            tool_call_id: tool_call.id,
            success: true,
            output: Some("Basic execution completed".to_string()),
            error: None,
            duration_ms: 100,
        })
    }
    
    async fn cancel_tool_call(&self, _tool_call_id: &ToolCallId) -> Result<()> {
        // 基础实现
        Ok(())
    }
}

/// 增强Agent连接实现 - 提供完整能力
pub struct EnhancedAgentConnection {
    basic: BasicAgentConnection,
    permission_manager: Arc<dyn PermissionCapability>,
    resource_manager: Arc<dyn ResourceCapability>,
    progress_reporter: Arc<dyn ProgressCapability>,
}

impl EnhancedAgentConnection {
    pub fn new(
        permission_manager: Arc<dyn PermissionCapability>,
        resource_manager: Arc<dyn ResourceCapability>,
        progress_reporter: Arc<dyn ProgressCapability>,
    ) -> Self {
        Self {
            basic: BasicAgentConnection::new(),
            permission_manager,
            resource_manager,
            progress_reporter,
        }
    }
}

#[async_trait]
impl AgentConnection for EnhancedAgentConnection {
    async fn execute_tool_call(&self, tool_call: ToolCall) -> Result<ToolCallResult> {
        // 如果需要权限，先检查权限
        if tool_call.requires_permission {
            let _permission = self.permission_manager
                .check_permission(&tool_call.tool_name)
                .await?;
        }
        
        // 报告进度
        self.progress_reporter
            .report_progress(&tool_call.id, ProgressUpdate {
                message: "Starting tool execution".to_string(),
                percentage: Some(0.0),
                stage: "initialization".to_string(),
            })
            .await?;
        
        // 执行工具调用
        let result = self.basic.execute_tool_call(tool_call.clone()).await?;
        
        // 报告完成
        self.progress_reporter
            .report_progress(&tool_call.id, ProgressUpdate {
                message: "Tool execution completed".to_string(),
                percentage: Some(100.0),
                stage: "completed".to_string(),
            })
            .await?;
        
        Ok(result)
    }
    
    async fn cancel_tool_call(&self, tool_call_id: &ToolCallId) -> Result<()> {
        self.basic.cancel_tool_call(tool_call_id).await
    }
    
    fn permission_manager(&self) -> Option<Arc<dyn PermissionCapability>> {
        Some(self.permission_manager.clone())
    }
    
    fn resource_manager(&self) -> Option<Arc<dyn ResourceCapability>> {
        Some(self.resource_manager.clone())
    }
    
    fn progress_reporter(&self) -> Option<Arc<dyn ProgressCapability>> {
        Some(self.progress_reporter.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_basic_agent_capabilities() {
        let agent = BasicAgentConnection::new();
        
        // 基础Agent不应该有可选能力
        assert!(!has_permission_capability(&agent));
        assert!(!has_resource_capability(&agent));
        assert!(!has_progress_capability(&agent));
        assert!(!has_environment_capability(&agent));
        assert!(!has_model_selection_capability(&agent));
        assert!(!has_session_capability(&agent));
        
        // 但应该能执行基础工具调用
        let tool_call = ToolCall {
            id: ToolCallId::new(),
            tool_name: "test_tool".to_string(),
            parameters: serde_json::json!({}),
            requires_permission: false,
        };
        
        let result = agent.execute_tool_call(tool_call).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_shell_type_detection() {
        let shell_type = ShellType::from_env();
        
        match shell_type {
            ShellType::Bash => assert_eq!(shell_type.command_prefix(), "bash -c"),
            ShellType::Zsh => assert_eq!(shell_type.command_prefix(), "bash -c"),
            ShellType::PowerShell => assert_eq!(shell_type.command_prefix(), "pwsh -Command"),
            ShellType::Cmd => assert_eq!(shell_type.command_prefix(), "cmd /C"),
            ShellType::Fish => assert_eq!(shell_type.command_prefix(), "bash -c"),
            ShellType::Other(_) => assert_eq!(shell_type.command_prefix(), "sh -c"),
        }
    }

    #[test]
    fn test_session_id_generation() {
        let session1 = SessionId::new();
        let session2 = SessionId::new();
        
        assert_ne!(session1.0, session2.0);
        assert!(!session1.0.is_empty());
        assert!(!session2.0.is_empty());
    }
}