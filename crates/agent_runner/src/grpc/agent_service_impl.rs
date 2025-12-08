//! AgentService gRPC 服务实现
//!
//! 实现 `agent.AgentService` 定义的所有 RPC 方法

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use shared_types::ModelProviderConfig;
use shared_types::grpc::{
    CancelRequest, CancelResponse, CancelResultType, ChatRequest as GrpcChatRequest,
    ChatResponse as GrpcChatResponse, GetStatusRequest, GetStatusResponse,
    ModelProviderConfig as GrpcModelProviderConfig, ProgressEvent, ProgressRequest,
    agent_service_server::AgentService,
};
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, instrument, warn};

use crate::model::AgentStatus;
use crate::proxy_agent::{LocalSetAgentRequest, PROJECT_AND_AGENT_INFO_MAP};
use crate::router::AppState;
use crate::service::SESSION_CACHE;
use shared_types::ChatPromptBuilder;

/// 将 gRPC ModelProviderConfig 转换为内部 ModelProviderConfig
fn convert_model_provider(grpc_config: GrpcModelProviderConfig) -> ModelProviderConfig {
    ModelProviderConfig {
        id: uuid::Uuid::new_v4().to_string(), // 生成唯一 ID
        name: grpc_config.provider,
        base_url: grpc_config.api_base.unwrap_or_default(),
        api_key: grpc_config.api_key.unwrap_or_default(),
        requires_openai_auth: true, // 默认值
        default_model: grpc_config.model,
        api_protocol: None, // 从 provider 名称推断
    }
}

/// gRPC AgentService 实现
pub struct AgentServiceImpl {
    app_state: Arc<AppState>,
}

impl AgentServiceImpl {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self { app_state }
    }
}

#[tonic::async_trait]
impl AgentService for AgentServiceImpl {
    /// 聊天对话接口 - 复用现有 handle_chat 核心逻辑
    #[instrument(skip(self, request))]
    async fn chat(
        &self,
        request: Request<GrpcChatRequest>,
    ) -> Result<Response<GrpcChatResponse>, Status> {
        let req = request.into_inner();

        info!(
            "🚀 [gRPC] Chat 请求: project_id={}, session_id={}, prompt={}",
            req.project_id, req.session_id, req.prompt
        );

        // 验证 prompt 不能为空
        if req.prompt.trim().is_empty() {
            return Err(Status::invalid_argument("prompt 字段不能为空"));
        }

        let project_id = if req.project_id.is_empty() {
            uuid::Uuid::new_v4().to_string().replace("-", "")
        } else {
            req.project_id.clone()
        };

        let session_id = if req.session_id.is_empty() {
            None
        } else {
            Some(req.session_id.clone())
        };

        // 检查 Agent 状态，禁止并发请求
        if let Some(agent_info) = PROJECT_AND_AGENT_INFO_MAP.get(&project_id) {
            if agent_info.status == AgentStatus::Active {
                return Err(Status::failed_precondition(
                    "Agent正在执行任务，请等待当前任务完成后再发送新请求",
                ));
            }
        }

        // 清理旧 session
        if let Some(ref sid) = session_id {
            if SESSION_CACHE.remove(sid).is_some() {
                info!("🗑️ [gRPC] 移除旧session: session_id={}", sid);
            }
        }

        // 获取或创建项目工作目录
        let workspace_dir = std::path::PathBuf::from("./project_workspace");
        let project_dir = workspace_dir.join(&project_id);
        if !project_dir.exists() {
            tokio::fs::create_dir_all(&project_dir)
                .await
                .map_err(|e| Status::internal(format!("创建项目目录失败: {}", e)))?;
        }

        // 生成 request_id
        let request_id = req
            .request_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string().replace("-", ""));

        // 构建 ChatPrompt
        let chat_prompt = ChatPromptBuilder::default()
            .project_id(project_id.clone())
            .project_path(project_dir)
            .session_id(session_id.clone())
            .prompt(req.prompt)
            .attachments(vec![]) // TODO: 转换 gRPC Attachment 到内部类型
            .data_source_attachments(req.data_source_attachments)
            .service_type(shared_types::ServiceType::RCoder)
            .request_id(request_id.clone())
            .build()
            .map_err(|e| Status::internal(format!("构建 ChatPrompt 失败: {}", e)))?;

        // 转换为 PromptMessage
        let prompt_message = agent_abstraction::PromptMessage::from(chat_prompt);

        // 转换 model_provider
        let model_provider = req.model_config.map(convert_model_provider);

        if let Some(ref provider) = model_provider {
            debug!(
                "📝 [gRPC] 使用模型配置: provider={}, model={}, base_url={}",
                provider.name, provider.default_model, provider.base_url
            );
        } else {
            warn!("⚠️ [gRPC] 未提供模型配置，将使用环境变量或默认配置");
        }

        let (local_task_request, chat_prompt_rx) =
            LocalSetAgentRequest::new(prompt_message, model_provider);

        self.app_state
            .local_task_sender
            .send(local_task_request)
            .map_err(|e| Status::internal(format!("发送任务失败: {}", e)))?;

        // 等待响应
        match chat_prompt_rx.await {
            Ok(response) => {
                let grpc_response = GrpcChatResponse {
                    project_id: response.project_id,
                    session_id: response.session_id,
                    success: response.error.is_none(),
                    error: response.error,
                    request_id: Some(request_id),
                };
                info!("✅ [gRPC] Chat 完成: success={}", grpc_response.success);
                Ok(Response::new(grpc_response))
            }
            Err(e) => {
                error!("❌ [gRPC] Chat 失败: {}", e);
                Err(Status::internal(format!("处理请求失败: {}", e)))
            }
        }
    }

    /// 订阅会话进度流 - Server Streaming RPC
    type SubscribeProgressStream =
        Pin<Box<dyn Stream<Item = Result<ProgressEvent, Status>> + Send>>;

    #[instrument(skip(self, request))]
    async fn subscribe_progress(
        &self,
        request: Request<ProgressRequest>,
    ) -> Result<Response<Self::SubscribeProgressStream>, Status> {
        let req = request.into_inner();
        let session_id = req.session_id.clone();

        info!(
            "📡 [gRPC] SubscribeProgress 开始: session_id={}",
            session_id
        );

        let (tx, rx) = mpsc::channel::<Result<ProgressEvent, Status>>(100);
        let session_id_clone = session_id.clone();

        // 启动后台任务来转发事件
        tokio::spawn(async move {
            // 🎯 关键修复：主动创建 SessionData 并插入 SESSION_CACHE
            // gRPC 流程中不会调用 HTTP endpoint，所以需要在这里创建
            use dashmap::mapref::entry::Entry;

            let session_data = match SESSION_CACHE.entry(session_id_clone.clone()) {
                Entry::Occupied(entry) => {
                    info!(
                        "📦 [gRPC] SESSION_CACHE 已存在，复用: session_id={}",
                        session_id_clone
                    );
                    entry.get().clone()
                }
                Entry::Vacant(entry) => {
                    info!(
                        "🆕 [gRPC] SESSION_CACHE 不存在，创建新的: session_id={}",
                        session_id_clone
                    );
                    let session_data = crate::service::SessionData::new(1000);
                    entry.insert(session_data.clone());
                    session_data
                }
            };

            // 创建新连接获取 receiver
            match session_data.create_new_connection(100).await {
                Ok((mut message_rx, cancellation_token)) => {
                    info!("📡 [gRPC] 成功创建 session 连接: {}", session_id_clone);

                    // 持续接收消息并转换为 ProgressEvent
                    loop {
                        tokio::select! {
                            _ = cancellation_token.cancelled() => {
                                debug!("📡 [gRPC] Session 连接被取消: {}", session_id_clone);
                                break;
                            }
                            msg = message_rx.recv() => {
                                match msg {
                                    Some(unified_message) => {
                                        let event = unified_message_to_progress_event(&unified_message);
                                        if tx.send(Ok(event)).await.is_err() {
                                            debug!("📡 [gRPC] 客户端已断开连接");
                                            break;
                                        }
                                    }
                                    None => {
                                        debug!("📡 [gRPC] Session 消息通道已关闭");
                                        break;
                                    }
                                }
                            }
                            // ✅ 新增：定期发送心跳，防止连接被中间网络设备断开
                            _ = tokio::time::sleep(Duration::from_secs(30)) => {
                                let heartbeat = ProgressEvent {
                                    message_type: "Heartbeat".to_string(),
                                    sub_type: "ping".to_string(),
                                    payload: r#"{"type":"heartbeat","message":"keep-alive"}"#.to_string(),
                                    request_id: None,
                                    timestamp: chrono::Utc::now().timestamp_millis(),
                                };

                                if tx.send(Ok(heartbeat)).await.is_err() {
                                    debug!("📡 [gRPC] 发送心跳失败，客户端已断开连接");
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("⚠️ [gRPC] 创建 session 连接失败: {}", e);
                    let _ = tx
                        .send(Err(Status::internal(format!("创建连接失败: {}", e))))
                        .await;
                }
            }
        });

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(
            Box::pin(stream) as Self::SubscribeProgressStream
        ))
    }

    /// 取消会话任务
    #[instrument(skip(self, request))]
    async fn cancel_session(
        &self,
        request: Request<CancelRequest>,
    ) -> Result<Response<CancelResponse>, Status> {
        let req = request.into_inner();
        info!(
            "🛑 [gRPC] CancelSession: session_id={}, reason={}",
            req.session_id, req.reason
        );

        // TODO: 实现取消逻辑，复用现有 agent_session_cancel handler
        // 目前返回成功响应
        let response = CancelResponse {
            success: true,
            result: CancelResultType::CancelResultSuccess as i32,
            message: Some("取消请求已接收".to_string()),
        };

        Ok(Response::new(response))
    }

    /// 获取 Agent 状态
    #[instrument(skip(self, request))]
    async fn get_status(
        &self,
        request: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        let req = request.into_inner();
        info!("📊 [gRPC] GetStatus: project_id={}", req.project_id);

        let status = if let Some(agent_info) = PROJECT_AND_AGENT_INFO_MAP.get(&req.project_id) {
            match agent_info.status {
                AgentStatus::Active => "busy",
                AgentStatus::Idle => "idle",
                AgentStatus::Terminating => "busy",
            }
        } else {
            "idle"
        };

        let response = GetStatusResponse {
            status: status.to_string(),
        };

        Ok(Response::new(response))
    }
}

/// 将 UnifiedSessionMessage 转换为 gRPC ProgressEvent
///
/// 简化版：直接透传 ACP JSON，不做任何字段提取
///
/// 优势：
/// 1. 零字段丢失：完整保留 ACP 结构
/// 2. 零维护成本：ACP 协议更新时后端无需修改
/// 3. 前端灵活性：前端可按需解析任意字段
fn unified_message_to_progress_event(
    message: &shared_types::UnifiedSessionMessage,
) -> ProgressEvent {
    let timestamp = message.timestamp.timestamp_millis();

    // 直接透传，不做任何字段提取
    ProgressEvent {
        message_type: format!("{:?}", message.message_type),
        sub_type: message.sub_type.clone(),
        payload: serde_json::to_string(&message.data).unwrap_or_default(),
        request_id: message.data.get("request_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        timestamp,
    }
}
