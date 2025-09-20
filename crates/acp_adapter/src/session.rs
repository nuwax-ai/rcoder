//! 会话管理模块

use crate::{
    config::AcpConfig,
    connection::ConnectionManager,
    types::{
        PermissionRequest, PermissionResponse, SessionId, SessionMessage,
        SessionState, StreamUpdate, ToolCallId, ToolCallInfo, ToolCallState, UserMessageId,
    },
    AcpAdapterError, AcpResult,
};
use agent_client_protocol::{PermissionOption, PromptRequest, PromptResponse, ToolCall, ToolCallId as ProtocolToolCallId};
use serde::{Deserialize, Serialize};
use dashmap::DashMap;
use serde_json;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// 会话句柄
#[derive(Clone)]
pub struct SessionHandle {
    id: SessionId,
    session: Arc<Session>,
}

impl SessionHandle {
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub async fn state(&self) -> SessionState {
        self.session.state().await
    }

    pub async fn send_prompt(&self, request: PromptRequest) -> AcpResult<PromptResponse> {
        self.session.send_prompt(request).await
    }

    pub async fn cancel(&self) -> AcpResult<()> {
        self.session.cancel().await
    }

    pub async fn truncate(&self, message_id: UserMessageId) -> AcpResult<()> {
        self.session.truncate(message_id).await
    }

    pub async fn set_title(&self, title: String) -> AcpResult<()> {
        self.session.set_title(title).await
    }

    pub async fn get_messages(&self) -> Vec<SessionMessage> {
        self.session.get_messages().await
    }

    pub async fn get_tool_calls(&self) -> Vec<ToolCall> {
        let tool_call_infos = self.session.get_tool_calls().await;
        tool_call_infos.into_iter().map(|info| {
            // 将 ToolCallInfo 转换为 ToolCall
            ToolCall {
                id: agent_client_protocol::ToolCallId(info.id.as_str().into()),
                title: info.name.clone(),
                kind: Default::default(),
                status: match info.state {
                    ToolCallState::PendingAuthorization | ToolCallState::Authorized =>
                        agent_client_protocol::ToolCallStatus::Pending,
                    ToolCallState::InProgress =>
                        agent_client_protocol::ToolCallStatus::InProgress,
                    ToolCallState::Completed =>
                        agent_client_protocol::ToolCallStatus::Completed,
                    ToolCallState::Failed(_) =>
                        agent_client_protocol::ToolCallStatus::Failed,
                    ToolCallState::Rejected | ToolCallState::Canceled =>
                        agent_client_protocol::ToolCallStatus::Failed,
                },
                content: Vec::new(),
                locations: Vec::new(),
                raw_input: Some(info.arguments),
                raw_output: None,
                meta: None,
            }
        }).collect()
    }

    pub async fn subscribe_to_updates(&self) -> mpsc::Receiver<StreamUpdate> {
        self.session.subscribe_to_updates().await
    }

    pub async fn request_permission(
        &self,
        request: PermissionRequest,
    ) -> AcpResult<PermissionResponse> {
        self.session.request_permission(request).await
    }
}

/// 会话统计
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatistics {
    pub total_messages: usize,
    pub total_tool_calls: usize,
    pub successful_tool_calls: usize,
    pub failed_tool_calls: usize,
    pub start_time: std::time::SystemTime,
    pub last_activity: std::time::SystemTime,
    pub tokens_used: Option<u64>,
}

impl Default for SessionStatistics {
    fn default() -> Self {
        let now = std::time::SystemTime::now();
        Self {
            total_messages: 0,
            total_tool_calls: 0,
            successful_tool_calls: 0,
            failed_tool_calls: 0,
            start_time: now,
            last_activity: now,
            tokens_used: None,
        }
    }
}

/// 会话
pub struct Session {
    id: SessionId,
    config: Arc<AcpConfig>,
    connection_manager: Arc<ConnectionManager>,
    state: Arc<RwLock<SessionState>>,
    messages: Arc<DashMap<UserMessageId, SessionMessage>>,
    tool_calls: Arc<DashMap<ToolCallId, ToolCallInfo>>,
    stats: Arc<RwLock<SessionStatistics>>,
    update_senders: Arc<DashMap<String, mpsc::Sender<StreamUpdate>>>,
    permission_requests: Arc<DashMap<ToolCallId, PermissionRequest>>,
    title: Arc<RwLock<Option<String>>>,
    created_at: std::time::SystemTime,
}

impl Session {
    pub fn new(config: Arc<AcpConfig>) -> Self {
        let session_id = SessionId(Uuid::new_v4().to_string().into());
        let created_at = std::time::SystemTime::now();

        Self {
            id: session_id,
            config,
            connection_manager: Arc::new(ConnectionManager::new()),
            state: Arc::new(RwLock::new(SessionState::Initializing)),
            messages: Arc::new(DashMap::new()),
            tool_calls: Arc::new(DashMap::new()),
            stats: Arc::new(RwLock::new(SessionStatistics {
                start_time: created_at,
                last_activity: created_at,
                ..Default::default()
            })),
            update_senders: Arc::new(DashMap::new()),
            permission_requests: Arc::new(DashMap::new()),
            title: Arc::new(RwLock::new(None)),
            created_at,
        }
    }

    pub fn handle(&self) -> SessionHandle {
        SessionHandle {
            id: self.id.clone(),
            session: Arc::new(self.clone()),
        }
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub async fn state(&self) -> SessionState {
        self.state.read().await.clone()
    }

    pub async fn set_state(&self, new_state: SessionState) {
        let mut state = self.state.write().await;
        *state = new_state.clone();

        // 发送状态更新事件
        self.send_update(StreamUpdate::SessionStateChanged {
            session_id: self.id.clone(),
            new_state: new_state.clone(),
            message: None,
        }).await;
    }

    pub async fn send_prompt(&self, request: PromptRequest) -> AcpResult<PromptResponse> {
        // 设置状态为处理中
        self.set_state(SessionState::Prompting).await;

        // 生成用户消息ID
        let user_message_id = UserMessageId::new();

        // 提取提示文本
        let prompt_text = if let Some(content_block) = request.prompt.first() {
            match content_block {
                agent_client_protocol::ContentBlock::Text(text_content) => text_content.text.clone(),
                _ => String::new(),
            }
        } else {
            String::new()
        };

        // 添加用户消息
        self.add_message(SessionMessage::User {
            id: user_message_id.clone(),
            content: prompt_text.clone(),
            timestamp: std::time::SystemTime::now(),
        }).await;

        // 发送提示开始事件
        self.send_update(StreamUpdate::PromptStarted {
            session_id: self.id.clone(),
            prompt: prompt_text,
        }).await;

        // 获取连接并发送请求
        let connection = self.connection_manager.get_or_create_connection().await?;

        // 创建流处理器
        let (stream_connection, mut update_rx) = crate::connection::StreamConnection::new(connection);
        let update_tx = self.create_update_sender().await;

        // 启动更新转发任务
        let session_id = self.id.clone();
        tokio::spawn(async move {
            while let Some(update) = update_rx.recv().await {
                let _ = update_tx.send(update).await;
            }
        });

        // 发送提示
        let response = stream_connection.send_prompt(self.id.clone(), request).await;

        // 恢复状态
        self.set_state(SessionState::Connected).await;

        response
    }

    pub async fn cancel(&self) -> AcpResult<()> {
        // 发送取消请求 (简化版本)
        let cancel_request = serde_json::json!({
            "type": "cancel",
            "session_id": self.id.to_string()
        });

        if let Err(e) = self.connection_manager.send_message(serde_json::to_string(&cancel_request).unwrap()).await {
            error!("发送取消请求失败: {}", e);
        }

        self.set_state(SessionState::Connected).await;
        info!("会话 {} 已取消", self.id);
        Ok(())
    }

    pub async fn truncate(&self, message_id: UserMessageId) -> AcpResult<()> {
        // 移除指定消息之后的所有消息
        let mut messages_to_remove = Vec::new();
        let mut found = false;

        for entry in self.messages.iter() {
            if let SessionMessage::User { id, .. } = entry.value() {
                if id == &message_id {
                    found = true;
                }
            }

            if found {
                messages_to_remove.push(entry.key().clone());
            }
        }

        // 移除消息
        for message_id in messages_to_remove {
            self.messages.remove(&message_id);
        }

        info!("会话 {} 已截断到消息 {}", self.id, message_id);
        Ok(())
    }

    pub async fn set_title(&self, title: String) -> AcpResult<()> {
        let mut title_guard = self.title.write().await;
        *title_guard = Some(title.clone());
        drop(title_guard);

        info!("会话 {} 标题已设置为: {}", self.id, title);
        Ok(())
    }

    pub async fn get_title(&self) -> Option<String> {
        self.title.read().await.clone()
    }

    pub async fn get_messages(&self) -> Vec<SessionMessage> {
        self.messages
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    pub async fn get_tool_calls(&self) -> Vec<ToolCallInfo> {
        self.tool_calls
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    pub async fn subscribe_to_updates(&self) -> mpsc::Receiver<StreamUpdate> {
        let (tx, rx) = mpsc::channel(100);
        let subscriber_id = Uuid::new_v4().to_string();
        self.update_senders.insert(subscriber_id, tx);
        rx
    }

    pub async fn request_permission(
        &self,
        request: PermissionRequest,
    ) -> AcpResult<PermissionResponse> {
        // 存储权限请求
        self.permission_requests.insert(request.tool_call_id.clone(), request.clone());

        // 发送权限请求事件
        self.send_update(StreamUpdate::ToolCallStarted {
            session_id: self.id.clone(),
            tool_call_id: request.tool_call_id.clone(),
            tool_name: request.tool_name.clone(),
        }).await;

        // TODO: 实现实际的权限请求逻辑
        // 这里暂时返回默认允许
        Ok(PermissionResponse::Allow {
            option_id: agent_client_protocol::PermissionOptionId("default".into()),
        })
    }

    async fn add_message(&self, message: SessionMessage) {
        match &message {
            SessionMessage::User { id, .. } => {
                self.messages.insert(id.clone(), message);
            }
            SessionMessage::Assistant { .. } => {
                // 助手消息使用生成的ID
                let id = UserMessageId::new();
                self.messages.insert(id, message);
            }
            SessionMessage::System { .. } => {
                // 系统消息使用生成的ID
                let id = UserMessageId::new();
                self.messages.insert(id, message);
            }
            SessionMessage::ToolCallResult { .. } => {
                // 工具调用结果使用生成的ID
                let id = UserMessageId::new();
                self.messages.insert(id, message);
            }
            SessionMessage::Status { .. } => {
                // 状态更新使用生成的ID
                let id = UserMessageId::new();
                self.messages.insert(id, message);
            }
        }

        // 更新统计
        let mut stats = self.stats.write().await;
        stats.total_messages += 1;
        stats.last_activity = std::time::SystemTime::now();
    }

    async fn add_tool_call(&self, tool_call: ToolCallInfo) {
        self.tool_calls.insert(tool_call.id.clone(), tool_call.clone());

        // 更新统计
        let mut stats = self.stats.write().await;
        stats.total_tool_calls += 1;
        match tool_call.state {
            ToolCallState::Completed => {
                stats.successful_tool_calls += 1;
            }
            ToolCallState::Failed(_) => {
                stats.failed_tool_calls += 1;
            }
            _ => {}
        }
        stats.last_activity = std::time::SystemTime::now();
    }

    async fn create_update_sender(&self) -> mpsc::Sender<StreamUpdate> {
        let (tx, _rx) = mpsc::channel(100);
        let subscriber_id = Uuid::new_v4().to_string();
        self.update_senders.insert(subscriber_id.clone(), tx.clone());

        // 启动清理任务
        let senders = self.update_senders.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(3600)).await;
            senders.remove(&subscriber_id);
        });

        tx
    }

    async fn send_update(&self, update: StreamUpdate) {
        for entry in self.update_senders.iter() {
            let sender = entry.value();
            if let Err(e) = sender.send(update.clone()).await {
                warn!("发送更新失败: {}", e);
            }
        }
    }

    pub async fn handle_tool_call(&self, tool_call: ToolCall) -> AcpResult<()> {
        let tool_call_id: ToolCallId = tool_call.id.into();
        let tool_call_info = ToolCallInfo {
            id: tool_call_id.clone(),
            name: tool_call.title.clone(),
            arguments: tool_call.raw_input.clone().unwrap_or_default(),
            state: ToolCallState::PendingAuthorization,
            timestamp: std::time::SystemTime::now(),
        };

        self.add_tool_call(tool_call_info.clone()).await;

        // 如果需要权限请求
        if true { // TODO: 从配置中读取
            let permission_request = PermissionRequest {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_call.title.clone(),
                description: format!("执行工具: {}", tool_call.title),
                arguments: tool_call.raw_input.unwrap_or_default(),
            };

            match self.request_permission(permission_request).await? {
                PermissionResponse::Allow { option_id } => {
                    self.update_tool_call_state(&tool_call_id, ToolCallState::Authorized).await;
                    self.send_update(StreamUpdate::ToolCallStarted {
                        session_id: self.id.clone(),
                        tool_call_id: tool_call_id.clone(),
                        tool_name: tool_call.title.clone(),
                    }).await;
                }
                PermissionResponse::Deny => {
                    self.update_tool_call_state(&tool_call_id, ToolCallState::Rejected).await;
                    return Ok(());
                }
                _ => {
                    self.update_tool_call_state(&tool_call_id, ToolCallState::Authorized).await;
                }
            }
        }

        Ok(())
    }

    async fn update_tool_call_state(&self, tool_call_id: &ToolCallId, new_state: ToolCallState) {
        if let Some(mut entry) = self.tool_calls.get_mut(tool_call_id) {
            entry.state = new_state.clone();
            entry.timestamp = std::time::SystemTime::now();
        }
    }

    pub async fn get_stats(&self) -> SessionStatistics {
        self.stats.read().await.clone()
    }

    pub async fn clear_history(&self) -> AcpResult<()> {
        self.messages.clear();
        self.tool_calls.clear();

        // 重置统计
        let mut stats = self.stats.write().await;
        stats.total_messages = 0;
        stats.total_tool_calls = 0;
        stats.successful_tool_calls = 0;
        stats.failed_tool_calls = 0;
        stats.last_activity = std::time::SystemTime::now();

        info!("会话 {} 历史记录已清除", self.id);
        Ok(())
    }

    pub async fn save_to_file(&self, path: &std::path::Path) -> AcpResult<()> {
        let session_data = SessionData {
            id: self.id.clone(),
            title: self.get_title().await,
            messages: self.get_messages().await,
            tool_calls: self.get_tool_calls().await,
            stats: self.get_stats().await,
            created_at: self.created_at,
        };

        let json = serde_json::to_string_pretty(&session_data)
            .map_err(|e| AcpAdapterError::session(format!("序列化会话数据失败: {}", e)))?;

        tokio::fs::write(path, json).await
            .map_err(|e| AcpAdapterError::session(format!("保存会话文件失败: {}", e)))?;

        info!("会话 {} 已保存到: {:?}", self.id, path);
        Ok(())
    }

    pub async fn load_from_file(&self, path: &std::path::Path) -> AcpResult<()> {
        let content = tokio::fs::read_to_string(path).await
            .map_err(|e| AcpAdapterError::session(format!("读取会话文件失败: {}", e)))?;

        let session_data: SessionData = serde_json::from_str(&content)
            .map_err(|e| AcpAdapterError::session(format!("解析会话数据失败: {}", e)))?;

        // 恢复消息
        self.messages.clear();
        for message in session_data.messages {
            self.add_message(message).await;
        }

        // 恢复工具调用
        self.tool_calls.clear();
        for tool_call in session_data.tool_calls {
            self.add_tool_call(tool_call).await;
        }

        // 恢复标题
        if let Some(title) = session_data.title {
            self.set_title(title).await?;
        }

        // 恢复统计
        let mut stats = self.stats.write().await;
        *stats = session_data.stats;

        info!("会话 {} 已从文件加载: {:?}", self.id, path);
        Ok(())
    }
}

impl Clone for Session {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            config: self.config.clone(),
            connection_manager: self.connection_manager.clone(),
            state: self.state.clone(),
            messages: self.messages.clone(),
            tool_calls: self.tool_calls.clone(),
            stats: self.stats.clone(),
            update_senders: self.update_senders.clone(),
            permission_requests: self.permission_requests.clone(),
            title: self.title.clone(),
            created_at: self.created_at,
        }
    }
}

/// 会话数据（用于序列化）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionData {
    id: SessionId,
    title: Option<String>,
    messages: Vec<SessionMessage>,
    tool_calls: Vec<ToolCallInfo>,
    stats: SessionStatistics,
    created_at: std::time::SystemTime,
}

/// 会话管理器
pub struct SessionManager {
    sessions: Arc<DashMap<SessionId, Arc<Session>>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
        }
    }

    pub async fn create_session(&self, config: Arc<AcpConfig>) -> AcpResult<SessionHandle> {
        let session = Arc::new(Session::new(config));
        let handle = session.handle();

        self.sessions.insert(handle.id().clone(), session);

        info!("创建新会话: {}", handle.id());
        Ok(handle)
    }

    pub fn get_session(&self, session_id: &SessionId) -> Option<SessionHandle> {
        self.sessions.get(session_id)
            .map(|session| session.handle())
    }

    pub async fn remove_session(&self, session_id: &SessionId) -> AcpResult<()> {
        if let Some((_, session)) = self.sessions.remove(session_id) {
            // 发送关闭事件
            session.set_state(SessionState::Closed).await;
            info!("会话 {} 已移除", session_id);
        }
        Ok(())
    }

    pub async fn list_sessions(&self) -> Vec<SessionId> {
        self.sessions
            .iter()
            .map(|session| session.key().clone())
            .collect()
    }

    pub async fn get_all_session_stats(&self) -> Vec<(SessionId, SessionStatistics)> {
        self.sessions
            .iter()
            .map(|entry| {
                let session_id = entry.key().clone();
                let stats = futures::executor::block_on(async { entry.value().get_stats().await });
                (session_id, stats)
            })
            .collect()
    }

    pub async fn shutdown(&self) -> AcpResult<()> {
        // 关闭所有会话
        for session_id in self.list_sessions().await {
            self.remove_session(&session_id).await?;
        }

        info!("会话管理器已关闭");
        Ok(())
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_session_creation() {
        let config = Arc::new(AcpConfig::claude_code());
        let session = Session::new(config);
        let handle = session.handle();

        assert!(!handle.id().to_string().is_empty());
        assert_eq!(session.state().await, SessionState::Initializing);
    }

    #[tokio::test]
    async fn test_session_manager() {
        let manager = SessionManager::new();
        let config = Arc::new(AcpConfig::claude_code());

        let handle = manager.create_session(config).await.unwrap();
        assert!(!handle.id().to_string().is_empty());

        let retrieved = manager.get_session(handle.id()).unwrap();
        assert_eq!(retrieved.id(), handle.id());

        manager.remove_session(handle.id()).await.unwrap();
        assert!(manager.get_session(handle.id()).is_none());
    }

    #[tokio::test]
    async fn test_session_messaging() {
        let config = Arc::new(AcpConfig::claude_code());
        let session = Session::new(config);

        let initial_count = session.get_messages().await.len();

        // 添加测试消息
        let message = SessionMessage::User {
            id: UserMessageId::new(),
            content: "Hello".to_string(),
            timestamp: std::time::SystemTime::now(),
        };
        session.add_message(message).await;

        assert_eq!(session.get_messages().await.len(), initial_count + 1);
    }
}