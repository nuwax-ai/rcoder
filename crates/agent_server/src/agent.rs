//! Agent 管理模块
//!
//! 简化版的 Agent 管理器，用于 Docker 容器内的基本 Agent 服务

use crate::{
    config::AgentType,
    shutdown::ShutdownSignal,
    AgentServerResult,
};
use dashmap::DashMap;
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Agent 管理器
pub struct AgentManager {
    /// Agent 类型
    agent_type: AgentType,
    /// 项目 ID
    project_id: String,
    /// 会话状态映射
    sessions: DashMap<String, SessionState>,
    /// 关闭信号发送器
    shutdown_tx: mpsc::Sender<ShutdownSignal>,
}

/// 会话状态
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionState {
    pub session_id: String,
    pub status: SessionStatus,
    pub request_id: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub project_id: String,
    pub agent_type: crate::config::AgentType,
    pub last_activity: chrono::DateTime<chrono::Utc>,
    pub current_task_id: Option<String>,
    pub metadata: std::collections::HashMap<String, serde_json::Value>,
}

/// 会话状态枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum SessionStatus {
    Initializing,
    Idle,
    Processing,
    Completed,
    Cancelled,
    Error,
}

/// Agent 状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AgentStatus {
    NotStarted,
    Starting,
    Running,
    Stopping,
    Stopped,
    Error,
}

impl AgentManager {
    /// 创建新的 Agent 管理器
    pub fn new(agent_type: AgentType, project_id: String) -> (Self, mpsc::Receiver<ShutdownSignal>) {
        let (shutdown_tx, shutdown_rx) = mpsc::channel(100);

        let manager = Self {
            agent_type,
            project_id,
            sessions: DashMap::new(),
            shutdown_tx,
        };

        (manager, shutdown_rx)
    }

    /// 初始化 Agent
    pub async fn initialize(&self) -> AgentServerResult<()> {
        info!("初始化 Agent: {:?}, 项目: {}", self.agent_type, self.project_id);
        Ok(())
    }

    /// 启动 Agent
    pub async fn start_agent(&self) -> AgentServerResult<()> {
        info!("启动 Agent: {:?}", self.agent_type);
        Ok(())
    }

    /// 停止 Agent
    pub async fn stop_agent(&self) -> AgentServerResult<()> {
        info!("停止 Agent: {:?}", self.agent_type);

        // 发送关闭信号
        if let Err(e) = self.shutdown_tx.send(ShutdownSignal::Stop).await {
            warn!("发送关闭信号失败: {}", e);
        }

        // 清理所有会话
        self.sessions.clear();
        info!("已清理所有会话");
        Ok(())
    }

    /// 创建会话
    pub async fn create_session(&self, session_id: Option<String>) -> AgentServerResult<String> {
        let session_id = session_id.unwrap_or_else(|| Uuid::new_v4().to_string());

        self.sessions.insert(session_id.clone(), SessionState {
            session_id: session_id.clone(),
            status: SessionStatus::Initializing,
            request_id: None,
            created_at: chrono::Utc::now(),
            project_id: "default_project".to_string(), // TODO: 从配置获取
            agent_type: crate::config::AgentType::Claude, // TODO: 从配置获取
            last_activity: chrono::Utc::now(),
            current_task_id: None,
            metadata: std::collections::HashMap::new(),
        });

        info!("创建会话: {}", session_id);
        Ok(session_id)
    }

    /// 获取会话状态
    pub async fn get_session(&self, session_id: &str) -> Option<SessionState> {
        self.sessions.get(session_id).map(|entry| entry.clone())
    }

    /// 删除会话
    pub async fn remove_session(&self, session_id: &str) -> bool {
        let removed = self.sessions.remove(session_id).is_some();
        if removed {
            info!("删除会话: {}", session_id);
        }
        removed
    }

    /// 列出所有会话
    pub async fn list_sessions(&self) -> Vec<SessionState> {
        self.sessions.iter().map(|entry| entry.value().clone()).collect()
    }

    /// 模拟发送聊天请求
    pub async fn send_chat_request(
        &self,
        session_id: &str,
        _request: PromptRequest,
    ) -> AgentServerResult<oneshot::Receiver<PromptResponse>> {
        debug!("模拟发送聊天请求到会话: {}", session_id);

        // 更新会话状态
        if let Some(session) = self.sessions.get(session_id) {
            let mut new_session = session.clone();
            new_session.status = SessionStatus::Processing;
            new_session.request_id = Some("mock_request".to_string());
            self.sessions.insert(session_id.to_string(), new_session);
        }

        // 模拟异步响应
        let (tx, rx) = oneshot::channel();
        let response = PromptResponse {
            request_id: "mock_request".to_string(),
            content: vec![],
            ..Default::default()
        };

        if let Err(_) = tx.send(response) {
            warn!("发送模拟响应失败");
        }

        Ok(rx)
    }

    /// 模拟取消请求
    pub async fn cancel_request(&self, session_id: &str, _cancel_notification: CancelNotification) -> AgentServerResult<()> {
        info!("取消会话 {} 的请求", session_id);

        if let Some(session) = self.sessions.get(session_id) {
            let mut new_session = session.clone();
            new_session.status = SessionStatus::Cancelled;
            new_session.request_id = None;
            self.sessions.insert(session_id.to_string(), new_session);
        }

        Ok(())
    }

    /// 获取 Agent 状态
    pub async fn get_agent_status(&self) -> AgentServerResult<AgentStatus> {
        Ok(AgentStatus::Running)
    }

    /// 健康检查
    pub async fn health_check(&self) -> AgentServerResult<bool> {
        Ok(true)
    }

    /// 获取活跃会话数
    pub async fn get_active_sessions_count(&self) -> usize {
        self.sessions.len()
    }
}

/// 模拟的 PromptRequest 结构
#[derive(Debug)]
pub struct PromptRequest {
    pub request_id: String,
    pub content: Vec<ContentBlock>,
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

/// 模拟的 PromptResponse 结构
#[derive(Debug)]
pub struct PromptResponse {
    pub request_id: String,
    pub content: Vec<ContentBlock>,
}

impl Default for PromptResponse {
    fn default() -> Self {
        Self {
            request_id: "default".to_string(),
            content: vec![],
        }
    }
}

/// 模拟的 ContentBlock
#[derive(Debug)]
pub enum ContentBlock {
    Text(TextContent),
}

/// 模拟的 TextContent
#[derive(Debug)]
pub struct TextContent {
    pub text: String,
}

/// 模拟的 CancelNotification
#[derive(Debug)]
pub struct CancelNotification {
    pub session_id: String,
    pub request_id: String,
    pub reason: String,
}