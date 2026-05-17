//! Agent Runner 本地服务实现
//!
//! 实现 `AgentHttpService` trait，直接调用本地服务（AGENT_REGISTRY, SESSION_CACHE, handle_chat_core）
//! 适用于 Agent Runner 的 HTTP Server 模式

use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::schema::{CancelNotification, SessionId};
use async_trait::async_trait;
use dashmap::DashMap;
use shared_types::{
    AgentStatusResponse, ChatResponse, HttpResult, RcoderChatRequest, ServiceType,
    agent_http_service::AgentHttpService,
    rcoder_agent_types::{
        RcoderAgentCancelRequest, RcoderAgentCancelResponse, RcoderAgentStopRequest,
        RcoderAgentStopResponse,
    },
};
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::service::{
    AGENT_REGISTRY, AgentSessionService, SESSION_CACHE, SessionData,
    chat_handler::{ChatHandlerContext, ChatHandlerInput},
    handle_chat_core,
};

/// Agent Runner 本地服务实现
///
/// 直接调用本地 AGENT_REGISTRY、SESSION_CACHE、handle_chat_core()
/// 不需要 gRPC 转发
pub struct LocalAgentHttpService {
    /// Agent 会话服务
    pub agent_session_service: Arc<AgentSessionService>,
    /// 共享 API Key 管理器
    pub shared_api_key_manager: Arc<DashMap<String, shared_types::ModelProviderConfig>>,
    /// project_id -> UUID 映射
    pub project_uuid_map: Arc<DashMap<String, String>>,
    /// 项目工作目录根路径
    pub projects_dir: PathBuf,
}

impl LocalAgentHttpService {
    /// 创建新的 LocalAgentHttpService
    pub fn new(
        agent_session_service: Arc<AgentSessionService>,
        shared_api_key_manager: Arc<DashMap<String, shared_types::ModelProviderConfig>>,
        project_uuid_map: Arc<DashMap<String, String>>,
        projects_dir: PathBuf,
    ) -> Self {
        Self {
            agent_session_service,
            shared_api_key_manager,
            project_uuid_map,
            projects_dir,
        }
    }
}

#[async_trait]
impl AgentHttpService for LocalAgentHttpService {
    /// Chat 对话请求
    async fn chat(&self, request: RcoderChatRequest) -> HttpResult<ChatResponse> {
        // 0. 验证 prompt 不为空
        if request.prompt.is_empty() {
            warn!("[LocalAgent] Empty prompt received");
            return HttpResult::error(
                shared_types::error_codes::ERR_VALIDATION,
                "Prompt cannot be empty",
            );
        }

        info!(
            "📨 [LocalAgent] Chat request: prompt_len={}, project_id={:?}, session_id={:?}",
            request.prompt.len(),
            request.project_id,
            request.session_id
        );

        // 1. 生成 project_id（如果未提供）
        let project_id = request
            .project_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string().replace('-', ""));

        // 2. 自动查找现有 session_id（如果未提供）
        let session_id = request.session_id.or_else(|| {
            AGENT_REGISTRY
                .get_agent_info(&project_id)
                .map(|info| info.session_id.to_string())
        });

        // 3. 创建项目工作目录
        let project_dir = self.projects_dir.join(&project_id);
        if let Err(e) = tokio::fs::create_dir_all(&project_dir).await {
            let error_msg = format!("Failed to create project dir: {}", e);
            warn!("[LocalAgent] {}", error_msg);
            return HttpResult::error(
                shared_types::error_codes::ERR_INTERNAL_SERVER_ERROR,
                &error_msg,
            );
        }

        // 4. 生成 request_id（如果未提供）
        let request_id = request
            .request_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        // 5. 构建 ChatHandlerInput
        let input = ChatHandlerInput {
            project_id: project_id.clone(),
            project_dir,
            session_id,
            prompt: request.prompt,
            request_id: request_id.clone(),
            attachments: request.attachments,
            data_source_attachments: request.data_source_attachments,
            model_config: request.model_provider,
            service_type: ServiceType::RCoder,
            user_id: None,
            agent_config_override: request.agent_config,
            system_prompt_override: request.system_prompt,
            user_prompt_template_override: request.user_prompt,
        };

        // 6. 构建 ChatHandlerContext
        let context = ChatHandlerContext {
            agent_session_service: self.agent_session_service.clone(),
            shared_api_key_manager: self.shared_api_key_manager.clone(),
            project_uuid_map: self.project_uuid_map.clone(),
        };

        // 7. 调用 handle_chat_core
        let output = handle_chat_core(input, &context).await;

        // 8. 将 session 写入 SESSION_CACHE（SSE 进度流需要）
        let session_id_str = output.session_id.clone();
        if SESSION_CACHE.get(&session_id_str).is_none() {
            let session_data = SessionData::new(1000);
            SESSION_CACHE.insert(session_id_str, session_data);
        }

        // 9. 构建响应
        if output.error.is_some() || !output.success {
            let error_code = output
                .error_code
                .as_deref()
                .unwrap_or(shared_types::error_codes::ERR_INTERNAL_SERVER_ERROR);
            let error_msg = output.error.unwrap_or_else(|| "Unknown error".to_string());
            HttpResult::error(error_code, &error_msg)
        } else {
            HttpResult::success(ChatResponse {
                project_id: output.project_id,
                session_id: output.session_id,
                error: output.error,
                request_id: Some(request_id),
                need_fallback: None,
                fallback_reason: None,
            })
        }
    }

    /// 查询 Agent 状态
    async fn get_status(&self, project_id: &str) -> HttpResult<AgentStatusResponse> {
        info!("🔍 [LocalAgent] Status query: project_id={}", project_id);

        if let Some(info) = AGENT_REGISTRY.get_agent_info(project_id) {
            // Agent 存在且活跃
            let response = AgentStatusResponse {
                project_id: project_id.to_string(),
                is_alive: true,
                session_id: Some(info.session_id.to_string()),
                status: Some(info.status),
                last_activity: Some(info.last_activity),
                created_at: Some(info.created_at),
                model_provider: None, // AgentRegistry 不存储 model_provider
            };

            info!(
                "✅ [LocalAgent] Agent status: project_id={}, is_alive=true, status={:?}",
                project_id, info.status
            );

            HttpResult::success(response)
        } else {
            // Agent 不存在
            info!("📭 [LocalAgent] Agent not found: project_id={}", project_id);

            let response = AgentStatusResponse {
                project_id: project_id.to_string(),
                is_alive: false,
                session_id: None,
                status: None,
                last_activity: None,
                created_at: None,
                model_provider: None,
            };

            HttpResult::success(response)
        }
    }

    /// 停止 Agent
    async fn stop(&self, request: RcoderAgentStopRequest) -> HttpResult<RcoderAgentStopResponse> {
        info!(
            "🛑 [LocalAgent] Stop request: project_id={}",
            request.project_id
        );

        // 1. 获取 Agent 信息
        if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(&request.project_id) {
            let session_id = agent_info.session_id.to_string();
            let cancel_tx = agent_info.cancel_tx.clone();

            // 释放读锁
            drop(agent_info);

            // 2. 发送取消信号（如果 channel 仍然打开）
            if !cancel_tx.is_closed() {
                let session_id_obj = SessionId::new(Arc::from(session_id.as_str()));
                let cancel_notification = CancelNotification::new(session_id_obj);

                let (result_tx, _result_rx) = oneshot::channel();
                let cancel_request = shared_types::CancelNotificationRequestWrapper {
                    cancel_notification,
                    result_tx,
                };

                match cancel_tx.send(cancel_request).await {
                    Ok(_) => {
                        info!("[LocalAgent] Cancel signal sent: session_id={}", session_id);
                    }
                    Err(e) => {
                        warn!(
                            "⚠️  [LocalAgent] Failed to send cancel signal: session_id={}, error={}",
                            session_id, e
                        );
                    }
                }
            }

            // 3. 从 AGENT_REGISTRY 移除 Agent
            AGENT_REGISTRY.remove_by_project(&request.project_id);

            info!(
                "✅ [LocalAgent] Agent stopped: project_id={}",
                request.project_id
            );

            HttpResult::success(RcoderAgentStopResponse {
                success: true,
                project_id: request.project_id,
                session_id: Some(session_id),
                message: "Agent stopped successfully".to_string(),
            })
        } else {
            // Agent 不存在，幂等返回成功
            info!(
                "ℹ️  [LocalAgent] Agent not found, returning success idempotently: project_id={}",
                request.project_id
            );

            HttpResult::success(RcoderAgentStopResponse {
                success: true,
                project_id: request.project_id,
                session_id: None,
                message: "Agent not found".to_string(),
            })
        }
    }

    /// 取消正在执行的任务
    async fn cancel(
        &self,
        request: RcoderAgentCancelRequest,
    ) -> HttpResult<RcoderAgentCancelResponse> {
        info!(
            "🚫 [LocalAgent] Cancel request: project_id={}, session_id={:?}",
            request.project_id, request.session_id
        );

        // 1. 查找 session_id（如果未提供，从 AGENT_REGISTRY 获取）
        let session_id = if let Some(sid) = request.session_id {
            sid
        } else {
            match AGENT_REGISTRY.get_agent_info(&request.project_id) {
                Some(info) => info.session_id.to_string(),
                None => {
                    // Agent 不存在，幂等返回成功
                    info!(
                        "ℹ️  [LocalAgent] Agent not found, returning success idempotently: project_id={}",
                        request.project_id
                    );
                    return HttpResult::success(RcoderAgentCancelResponse {
                        success: true,
                        session_id: String::new(),
                    });
                }
            }
        };

        // 2. 获取 Agent 信息并发送取消信号
        if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(&request.project_id) {
            let cancel_tx = agent_info.cancel_tx.clone();

            // 释放读锁
            drop(agent_info);

            // 检查是否已经空闲或停止中（幂等性）
            if cancel_tx.is_closed() {
                info!(
                    "ℹ️  [LocalAgent] Agent stopped, cancel channel is closed: session_id={}",
                    session_id
                );
            } else {
                // 创建取消通知
                let session_id_obj = SessionId::new(Arc::from(session_id.as_str()));
                let cancel_notification = CancelNotification::new(session_id_obj);

                // 创建 oneshot channel 接收取消结果（HTTP 不等待结果，直接丢弃）
                let (result_tx, _result_rx) = oneshot::channel();
                let cancel_request = shared_types::CancelNotificationRequestWrapper {
                    cancel_notification,
                    result_tx,
                };

                // 发送取消信号（异步）
                match cancel_tx.send(cancel_request).await {
                    Ok(_) => {
                        info!("[LocalAgent] Cancel signal sent: session_id={}", session_id);
                    }
                    Err(e) => {
                        warn!(
                            "⚠️  [LocalAgent] Failed to send cancel signal: session_id={}, error={}",
                            session_id, e
                        );
                    }
                }
            }
        } else {
            // Session 不存在，幂等返回成功
            info!(
                "ℹ️  [LocalAgent] Agent not found, returning success idempotently: session_id={}",
                session_id
            );
        }

        HttpResult::success(RcoderAgentCancelResponse {
            success: true,
            session_id,
        })
    }
}
