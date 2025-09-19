use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use chrono;

use agent_client_protocol as acp;
use agent_servers::{AgentServer, AgentConnection};
use acp_thread::{AgentConnection as AcpThreadConnection, AcpThread};

/// HTTP友好的Agent适配器
pub struct HttpNativeAgent {
    connections: Arc<RwLock<HashMap<Uuid, Arc<dyn AgentConnection>>>>,
    sessions: Arc<RwLock<HashMap<Uuid, HttpSession>>>,
    working_dir: PathBuf,
}

/// HTTP会话信息
pub struct HttpSession {
    pub id: Uuid,
    pub project_path: PathBuf,
    pub acp_thread: Option<Arc<AcpThread>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub context_size: usize,
    pub max_context_size: usize,
    pub message_count: usize,
    pub token_usage: TokenUsage,
    pub model_info: Option<ModelInfo>,
    pub capabilities: Option<acp::PromptCapabilities>,
}

impl HttpNativeAgent {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            working_dir: std::env::current_dir().unwrap_or_default(),
        }
    }

    pub async fn create_session(&self, project_path: PathBuf) -> Result<Uuid> {
        let session_id = Uuid::new_v4();

        info!("Creating HTTP session {} for project: {}", session_id, project_path.display());

        // 确保项目目录存在
        tokio::fs::create_dir_all(&project_path).await?;

        // 创建 ACP 连接
        let acp_thread = match self.create_acp_connection(&project_path).await {
            Ok(thread) => Some(Arc::new(thread)),
            Err(e) => {
                warn!("Failed to create ACP connection: {}", e);
                None
            }
        };

        // 获取模型信息 - 从Claude Code配置中读取，不使用默认值
        let (model_info, capabilities) = if let Some(ref thread) = acp_thread {
            self.extract_model_info_from_thread(thread).await?
        } else {
            // 如果没有ACP连接，不设置默认模型信息
            (None, None)
        };

        // 从模型信息中获取上下文限制和模型名称，如果没有则使用合理的默认值
        let max_context_size = model_info.as_ref()
            .map(|m| m.max_tokens as usize)
            .unwrap_or(200000); // 如果没有模型信息，使用合理的默认值

        let model_name = model_info.as_ref()
            .map(|m| m.name.clone())
            .unwrap_or_else(|| "unknown".to_string());

        let session = HttpSession {
            id: session_id,
            project_path: project_path.clone(),
            acp_thread,
            created_at: chrono::Utc::now(),
            context_size: 0,
            max_context_size,
            message_count: 0,
            token_usage: TokenUsage::new(0, 0, max_context_size as u32, model_name),
            model_info,
            capabilities,
        };

        self.sessions.write().await.insert(session_id, session);
        info!("HTTP session {} created successfully", session_id);
        Ok(session_id)
    }

    async fn create_acp_connection(&self, project_path: &PathBuf) -> Result<AcpThread> {
        debug!("Creating ACP connection for project: {}", project_path.display());

        // Note: AcpThread::new requires a different signature than what we're trying to use
        // This method needs to be implemented properly with the correct ACP integration
        // For now, we'll return a placeholder error
        Err(anyhow::anyhow!("ACP connection not yet implemented - needs proper Zed integration"))
    }

    async fn extract_model_info_from_thread(&self, acp_thread: &AcpThread) -> Result<(Option<ModelInfo>, Option<acp::PromptCapabilities>)> {
        // 尝试从 ACP 线程获取模型信息
        // 注意：这里需要根据实际的 Zed ACP 线程 API 来实现

        // 获取提示能力
        let capabilities = self.get_prompt_capabilities_from_thread(acp_thread).await?;

        // 目前无法从 ACP 线程获取模型信息，需要等 Zed 集成完成
        let model_info = None;

        Ok((model_info, Some(capabilities)))
    }

    async fn get_prompt_capabilities_from_thread(&self, acp_thread: &AcpThread) -> Result<acp::PromptCapabilities> {
        // 这里需要调用 ACP 线程的方法来获取能力
        // 由于我们无法直接访问 Zed 的内部 API，这里提供一个默认实现

        // 在实际应用中，这里应该调用类似：
        // acp_thread.prompt_capabilities()

        // 暂时返回一个通用的能力集
        Ok(acp::PromptCapabilities {
            meta: None,
            image: true,  // 假设支持图片
            audio: false, // 假设不支持音频
            embedded_context: true, // 支持嵌入式上下文
        })
    }

    fn infer_model_info_from_capabilities(&self, capabilities: &acp::PromptCapabilities) -> Result<Option<ModelInfo>> {
        // 从 Claude Code 直接获取模型信息，不使用启发式推断
        // 这里应该调用 ACP 线程的 API 来获取真实的模型信息
        // 如果无法获取，返回 None 让调用者处理
        Ok(None)
    }



    async fn update_model_info_from_response(&self, session_id: Uuid, response: &acp::Message) -> Result<()> {
        // 从响应中提取模型信息并更新会话
        // 例如，响应中可能包含模型名称或 token 限制信息

        if let acp::MessageContent::Text { text } = &response.content {
            if let Some(model_info) = self.extract_model_info_from_text(text) {
                let mut sessions = self.sessions.write().await;
                if let Some(session) = sessions.get_mut(&session_id) {
                    session.model_info = Some(model_info);

                    // 更新最大上下文限制
                    if let Some(info) = &session.model_info {
                        session.max_context_size = info.max_tokens as usize;
                        session.token_usage.max_tokens = info.max_tokens;
                        session.token_usage.model_name = info.name.clone();
                    }

                    info!("Updated model info for session {}: {:?}", session_id, session.model_info);
                }
            }
        }

        Ok(())
    }

    fn extract_model_info_from_text(&self, text: &str) -> Option<ModelInfo> {
        // 尝试从响应文本中提取模型信息
        // 这可能出现在系统消息或元数据中

        // 查找模型信息的 JSON 块
        if let Some(start) = text.find('{') {
            if let Some(end) = text.rfind('}') {
                let json_str = &text[start..=end];
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
                    if let Some(model_name) = parsed.get("model").and_then(|m| m.as_str()) {
                        return Some(ModelInfo {
                            id: model_name.to_string(),
                            name: model_name.to_string(),
                            provider: "anthropic".to_string(),
                            max_tokens: self.get_model_token_limit(model_name).unwrap_or(200000),
                            supports_images: true,
                            supports_audio: false,
                            supports_cache: true,
                            capabilities: ModelCapabilities {
                                embedded_context: true,
                                image_support: true,
                                audio_support: false,
                                tool_use: true,
                                streaming: true,
                            },
                        });
                    }
                }
            }
        }

        None
    }

    pub async fn send_prompt(&self, session_id: Uuid, prompt: String) -> Result<PromptResponse> {
        debug!("Sending prompt to session {}: {}", session_id, prompt);

        let mut session = self.sessions.write().await.get_mut(&session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session_id))?;

        let acp_thread = session.acp_thread.as_ref()
            .ok_or_else(|| anyhow::anyhow!("ACP connection not initialized for session: {}", session_id))?;

        // 构建提示消息
        let prompt_message = self.build_prompt_message(&prompt).await?;

        // 发送提示到 Claude Code
        // Note: This needs to be adapted to the actual AcpThread API
        // For now, we'll create a placeholder response
        let response = self.create_placeholder_response(&prompt_message).await?;

        // 处理响应
        let prompt_response = self.process_response(session_id, response).await?;

        info!("Prompt processed successfully for session {}", session_id);
        Ok(prompt_response)
    }

    async fn build_prompt_message(&self, prompt: &str) -> Result<acp::Message> {
        use acp::MessageContent;

        let content = MessageContent::Text {
            text: prompt.to_string(),
        };

        Ok(acp::Message {
            id: Uuid::new_v4(),
            role: acp::Role::User,
            content,
            timestamp: chrono::Utc::now(),
        })
    }

    async fn create_placeholder_response(&self, _message: &acp::Message) -> Result<acp::Message> {
        // Create a placeholder response since we can't directly use AcpThread.send()
        // This should be replaced with proper AcpThread integration
        Ok(acp::Message {
            id: Uuid::new_v4(),
            role: acp::Role::Assistant,
            content: acp::MessageContent::Text {
                text: "Placeholder response - ACP integration needs to be implemented".to_string(),
            },
            timestamp: chrono::Utc::now(),
        })
    }

    async fn process_response(&self, session_id: Uuid, response: acp::Message) -> Result<PromptResponse> {
        use acp::MessageContent;

        let mut files_modified = Vec::new();
        let mut response_text = String::new();
        let mut token_usage = None;

        match response.content {
            MessageContent::Text { text } => {
                response_text = text;

                // 尝试从文本中提取 token 使用情况（JSON 格式）
                token_usage = self.extract_token_usage_from_text(&text);

                // 尝试从响应中提取模型信息
                self.update_model_info_from_response(session_id, &response).await?;
            }
            MessageContent::ToolResult { result, .. } => {
                // 处理工具结果，可能是文件修改
                if let Some(files) = result.get("files") {
                    if let Some(file_list) = files.as_array() {
                        for file in file_list {
                            if let Some(file_path) = file.get("path").and_then(|p| p.as_str()) {
                                files_modified.push(file_path.to_string());
                            }
                        }
                    }
                }
                response_text = result.get("output")
                    .and_then(|o| o.as_str())
                    .unwrap_or("Tool execution completed").to_string();

                // 尝试从工具结果中提取 token 使用情况
                token_usage = self.extract_token_usage_from_result(&result);
            }
            _ => {
                response_text = "Unsupported message type".to_string();
            }
        }

        // 更新会话的 token 使用统计
        if let Some(usage) = &token_usage {
            self.update_session_token_usage(session_id, usage).await?;
        }

        // 更新会话统计
        self.update_session_stats(session_id, &response).await?;

        Ok(PromptResponse {
            session_id: session_id.to_string(),
            response: response_text,
            status: "completed".to_string(),
            files_modified,
            token_usage,
        })
    }

    fn extract_token_usage_from_text(&self, text: &str) -> Option<TokenUsage> {
        // 尝试从响应文本中解析 JSON 格式的 token 使用情况
        // 例如: {"usage": {"input_tokens": 100, "output_tokens": 50}}
        if let Some(start) = text.find('{') {
            if let Some(end) = text.rfind('}') {
                let json_str = &text[start..=end];
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
                    if let Some(usage_obj) = parsed.get("usage") {
                        return self.parse_token_usage(usage_obj);
                    }
                }
            }
        }
        None
    }

    fn extract_token_usage_from_result(&self, result: &serde_json::Value) -> Option<TokenUsage> {
        // 从工具结果中提取 token 使用情况
        if let Some(usage_obj) = result.get("usage") {
            self.parse_token_usage(usage_obj)
        } else if let Some(metadata) = result.get("metadata") {
            if let Some(usage_obj) = metadata.get("usage") {
                self.parse_token_usage(usage_obj)
            } else {
                None
            }
        } else {
            None
        }
    }

    fn parse_token_usage(&self, usage_obj: &serde_json::Value) -> Option<TokenUsage> {
        let input_tokens = usage_obj.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let output_tokens = usage_obj.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let cache_tokens = usage_obj.get("cache_tokens").and_then(|v| v.as_u64()).map(|v| v as u32);

        // 获取模型信息
        let model_name = usage_obj.get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        let max_tokens = self.get_model_token_limit(model_name).unwrap_or(200000);

        if cache_tokens.is_some() {
            Some(TokenUsage::with_cache(input_tokens, output_tokens, cache_tokens.unwrap(), max_tokens, model_name.to_string()))
        } else {
            Some(TokenUsage::new(input_tokens, output_tokens, max_tokens, model_name.to_string()))
        }
    }

    async fn update_session_token_usage(&self, session_id: Uuid, usage: &TokenUsage) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            session.token_usage = TokenUsage::new(
                session.token_usage.input_tokens + usage.input_tokens,
                session.token_usage.output_tokens + usage.output_tokens,
                session.token_usage.max_tokens,
                session.token_usage.model_name.clone(),
            );

            // 更新上下文大小
            session.context_size = session.token_usage.total_tokens as usize;

            debug!("Updated token usage for session {}: {:?}", session_id, session.token_usage);
        }
        Ok(())
    }

    async fn update_session_stats(&self, session_id: Uuid, response: &acp::Message) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.get_mut(&session_id) {
            session.message_count += 1;

            // 检查上下文大小是否超过限制
            if session.context_size > session.max_context_size {
                warn!("Session {} context size {} exceeds limit {}",
                      session_id, session.context_size, session.max_context_size);

                // 这里可以实现上下文清理策略
                self.cleanup_session_context(session);
            }
        }
        Ok(())
    }

    fn cleanup_session_context(&self, session: &mut HttpSession) {
        // 简单的清理策略：如果超过限制，重置计数器
        // 在实际应用中，这里可以：
        // 1. 删除最旧的消息
        // 2. 压缩消息内容
        // 3. 保留重要的上下文

        if session.context_size > session.max_context_size {
            session.context_size = session.max_context_size;
            session.message_count = session.message_count.max(10) - 10; // 保留最近10条消息

            info!("Cleaned up session {} context, reduced to {} messages",
                  session.id, session.message_count);
        }
    }

    pub async fn get_session(&self, session_id: Uuid) -> Option<HttpSession> {
        self.sessions.read().await.get(&session_id).cloned()
    }

    pub async fn close_session(&self, session_id: Uuid) -> Result<()> {
        info!("Closing session: {}", session_id);

        if let Some(session) = self.sessions.write().await.remove(&session_id) {
            if let Some(_acp_thread) = session.acp_thread {
                // 关闭 ACP 连接
                // Note: AcpThread doesn't have a shutdown method
                // This should be implemented based on the actual ACP thread lifecycle
                debug!("ACP connection cleanup for session {} (placeholder)", session_id);
            }
        }

        info!("Session {} closed successfully", session_id);
        Ok(())
    }

    pub async fn list_sessions(&self) -> Vec<HttpSession> {
        self.sessions.read().await.values().cloned().collect()
    }

    pub async fn get_session_status(&self, session_id: Uuid) -> Result<SessionStatus> {
        let session = self.sessions.read().await.get(&session_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session_id))?;

        let status = if session.acp_thread.is_some() {
            "active"
        } else {
            "inactive"
        };

        let usage_ratio = if session.max_context_size > 0 {
            session.context_size as f32 / session.max_context_size as f32
        } else {
            0.0
        };

        Ok(SessionStatus {
            session_id: session_id.to_string(),
            status: status.to_string(),
            project_path: session.project_path.display().to_string(),
            created_at: session.created_at,
            context_size: session.context_size,
            max_context_size: session.max_context_size,
            message_count: session.message_count,
            usage_ratio,
            token_usage: session.token_usage.clone(),
        })
    }

    pub async fn get_session_statistics(&self, session_id: Uuid) -> Result<SessionStatistics> {
        let session = self.sessions.read().await.get(&session_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session_id))?;

        Ok(SessionStatistics {
            session_id: session_id.to_string(),
            project_path: session.project_path.display().to_string(),
            duration: chrono::Utc::now().signed_duration_since(session.created_at).num_seconds(),
            message_count: session.message_count,
            context_size: session.context_size,
            max_context_size: session.max_context_size,
            token_usage: session.token_usage.clone(),
            usage_ratio: if session.max_context_size > 0 {
                session.context_size as f32 / session.max_context_size as f32
            } else {
                0.0
            },
            average_tokens_per_message: if session.message_count > 0 {
                session.token_usage.total_tokens / session.message_count
            } else {
                0
            },
        })
    }

    pub async fn get_global_statistics(&self) -> GlobalStatistics {
        let sessions = self.sessions.read().await;
        let total_sessions = sessions.len();
        let active_sessions = sessions.values().filter(|s| s.acp_thread.is_some()).count();
        let total_messages: usize = sessions.values().map(|s| s.message_count).sum();
        let total_tokens: u32 = sessions.values().map(|s| s.token_usage.total_tokens).sum();

        let total_context_size: usize = sessions.values().map(|s| s.context_size).sum();
        let max_context_usage = sessions.values()
            .map(|s| if s.max_context_size > 0 { s.context_size as f32 / s.max_context_size as f32 } else { 0.0 })
            .fold(0.0, f32::max);

        GlobalStatistics {
            total_sessions,
            active_sessions,
            total_messages,
            total_tokens,
            total_context_size,
            max_context_usage,
            average_tokens_per_session: if total_sessions > 0 { total_tokens / total_sessions as u32 } else { 0 },
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PromptResponse {
    pub session_id: String,
    pub response: String,
    pub status: String,
    pub files_modified: Vec<String>,
    pub token_usage: Option<TokenUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_tokens: Option<u32>,
    pub total_tokens: u32,
    pub max_tokens: u32,
    pub model_name: String,
}

impl TokenUsage {
    pub fn new(input_tokens: u32, output_tokens: u32, max_tokens: u32, model_name: String) -> Self {
        Self {
            input_tokens,
            output_tokens,
            cache_tokens: None,
            total_tokens: input_tokens + output_tokens,
            max_tokens,
            model_name,
        }
    }

    pub fn with_cache(input_tokens: u32, output_tokens: u32, cache_tokens: u32, max_tokens: u32, model_name: String) -> Self {
        Self {
            input_tokens,
            output_tokens,
            cache_tokens: Some(cache_tokens),
            total_tokens: input_tokens + output_tokens + cache_tokens,
            max_tokens,
            model_name,
        }
    }

    pub fn usage_ratio(&self) -> f32 {
        if self.max_tokens == 0 {
            0.0
        } else {
            self.total_tokens as f32 / self.max_tokens as f32
        }
    }

    pub fn usage_status(&self) -> TokenUsageStatus {
        let ratio = self.usage_ratio();
        if ratio >= 1.0 {
            TokenUsageStatus::Exceeded
        } else if ratio >= 0.8 {
            TokenUsageStatus::Warning
        } else {
            TokenUsageStatus::Normal
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TokenUsageStatus {
    Normal,
    Warning,
    Exceeded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub max_tokens: u32,
    pub supports_images: bool,
    pub supports_audio: bool,
    pub supports_cache: bool,
    pub capabilities: ModelCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub embedded_context: bool,
    pub image_support: bool,
    pub audio_support: bool,
    pub tool_use: bool,
    pub streaming: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionStatus {
    pub session_id: String,
    pub status: String,
    pub project_path: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub context_size: usize,
    pub max_context_size: usize,
    pub message_count: usize,
    pub usage_ratio: f32,
    pub token_usage: TokenUsage,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub project_path: PathBuf,
    pub initial_prompt: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SendMessageRequest {
    pub message: String,
    pub context: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionStatistics {
    pub session_id: String,
    pub project_path: String,
    pub duration: i64, // in seconds
    pub message_count: usize,
    pub context_size: usize,
    pub max_context_size: usize,
    pub token_usage: TokenUsage,
    pub usage_ratio: f32,
    pub average_tokens_per_message: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GlobalStatistics {
    pub total_sessions: usize,
    pub active_sessions: usize,
    pub total_messages: usize,
    pub total_tokens: u32,
    pub total_context_size: usize,
    pub max_context_usage: f32,
    pub average_tokens_per_session: u32,
}