//! AgentService gRPC 服务实现
//!
//! 实现 `agent.AgentService` 定义的所有 RPC 方法

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::{CancelNotification, SessionId};
use shared_types::ModelProviderConfig;
use shared_types::grpc::{
    CancelRequest, CancelResponse, CancelResultType, ChatAgentConfig as GrpcChatAgentConfig,
    ChatAgentServerConfig as GrpcChatAgentServerConfig,
    ChatContextServerConfig as GrpcChatContextServerConfig, ChatRequest as GrpcChatRequest,
    ChatResponse as GrpcChatResponse, GetContainerStatusRequest, GetContainerStatusResponse,
    GetStatusRequest, GetStatusResponse, ModelProviderConfig as GrpcModelProviderConfig,
    ProgressEvent, ProgressRequest, agent_service_server::AgentService, attachment,
    attachment_source,
};
use shared_types::{
    Attachment, AttachmentSource, AudioAttachment, DocumentAttachment, ImageAttachment,
    ImageDimensions, TextAttachment,
};
use shared_types::{ChatAgentConfig, ChatAgentServerConfig, ChatContextServerConfig};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, instrument, warn};

use crate::model::AgentStatus;
use crate::proxy_agent::LocalSetAgentRequest;
use crate::router::AppState;
use crate::service::{AGENT_REGISTRY, SESSION_CACHE};
use crate::{CancelNotificationRequestWrapper, CancelResult};
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

/// 将 gRPC ChatAgentConfig 转换为内部 ChatAgentConfig
fn convert_agent_config(grpc_config: GrpcChatAgentConfig) -> ChatAgentConfig {
    ChatAgentConfig {
        agent_server: grpc_config.agent_server.map(convert_agent_server_config),
        context_servers: grpc_config
            .context_servers
            .into_iter()
            .map(|(k, v)| (k, convert_context_server_config(v)))
            .collect(),
        resource_limits: None, // gRPC 消息中暂时不传递 resource_limits
    }
}

/// 将 gRPC ChatAgentServerConfig 转换为内部 ChatAgentServerConfig
fn convert_agent_server_config(grpc_config: GrpcChatAgentServerConfig) -> ChatAgentServerConfig {
    ChatAgentServerConfig {
        agent_id: grpc_config.agent_id,
        command: grpc_config.command,
        args: if grpc_config.args.is_empty() {
            None
        } else {
            Some(grpc_config.args)
        },
        env: if grpc_config.env.is_empty() {
            None
        } else {
            Some(grpc_config.env)
        },
        metadata: if grpc_config.metadata.is_empty() {
            None
        } else {
            Some(grpc_config.metadata)
        },
    }
}

/// 将 gRPC ChatContextServerConfig 转换为内部 ChatContextServerConfig
fn convert_context_server_config(
    grpc_config: GrpcChatContextServerConfig,
) -> ChatContextServerConfig {
    ChatContextServerConfig {
        source: grpc_config.source,
        enabled: grpc_config.enabled,
        command: grpc_config.command,
        args: if grpc_config.args.is_empty() {
            None
        } else {
            Some(grpc_config.args)
        },
        env: if grpc_config.env.is_empty() {
            None
        } else {
            Some(grpc_config.env)
        },
    }
}

/// 将 gRPC AttachmentSource 转换为内部 AttachmentSource
fn convert_attachment_source(
    grpc_source: Option<shared_types::grpc::AttachmentSource>,
) -> Option<AttachmentSource> {
    let source = grpc_source?.source?;
    Some(match source {
        attachment_source::Source::FilePath(path) => AttachmentSource::FilePath { path },
        attachment_source::Source::Base64(data) => AttachmentSource::Base64 {
            data: data.data,
            mime_type: data.mime_type,
        },
        attachment_source::Source::Url(url) => AttachmentSource::Url { url },
    })
}

/// 将 gRPC Attachment 转换为内部 Attachment
fn convert_attachment(grpc_attachment: shared_types::grpc::Attachment) -> Option<Attachment> {
    let attachment_type = grpc_attachment.attachment_type?;

    Some(match attachment_type {
        attachment::AttachmentType::Text(text) => Attachment::Text(TextAttachment {
            id: text.id,
            source: convert_attachment_source(text.source)?,
            filename: text.filename,
            description: text.description,
        }),
        attachment::AttachmentType::Image(image) => Attachment::Image(ImageAttachment {
            id: image.id,
            source: convert_attachment_source(image.source)?,
            mime_type: image.mime_type,
            filename: image.filename,
            description: image.description,
            dimensions: image.dimensions.map(|d| ImageDimensions {
                width: d.width,
                height: d.height,
            }),
        }),
        attachment::AttachmentType::Audio(audio) => Attachment::Audio(AudioAttachment {
            id: audio.id,
            source: convert_attachment_source(audio.source)?,
            mime_type: audio.mime_type,
            filename: audio.filename,
            description: audio.description,
            duration: audio.duration,
        }),
        attachment::AttachmentType::Document(doc) => Attachment::Document(DocumentAttachment {
            id: doc.id,
            source: convert_attachment_source(doc.source)?,
            mime_type: doc.mime_type,
            filename: doc.filename,
            description: doc.description,
            size: doc.size,
        }),
    })
}

/// 批量转换附件列表
fn convert_attachments(grpc_attachments: Vec<shared_types::grpc::Attachment>) -> Vec<Attachment> {
    grpc_attachments
        .into_iter()
        .filter_map(convert_attachment)
        .collect()
}

/// gRPC AgentService 实现
pub struct AgentServiceImpl {
    app_state: Arc<AppState>,
}

impl AgentServiceImpl {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self { app_state }
    }

    /// 获取活跃任务数
    ///
    /// 查询 AGENT_REGISTRY 中状态为 Active 的 Agent 数量
    fn get_active_tasks_count(&self) -> i32 {
        let count = AGENT_REGISTRY
            .iter_agents()
            .filter(|entry| entry.value().status == AgentStatus::Active)
            .count();

        count as i32
    }

    /// 获取容器运行时长（秒）
    ///
    /// 从进程启动时间计算到现在的时长
    fn get_uptime_seconds(&self) -> i64 {
        use std::time::SystemTime;

        // 使用进程启动时间作为容器启动时间的近似值
        // 注意：这是一个简化实现，实际的容器启动时间应该更早
        static START_TIME: std::sync::OnceLock<SystemTime> = std::sync::OnceLock::new();

        let start = START_TIME.get_or_init(|| SystemTime::now());

        match SystemTime::now().duration_since(*start) {
            Ok(duration) => duration.as_secs() as i64,
            Err(_) => {
                warn!("⚠️ [GET_CONTAINER_STATUS] 计算运行时长失败，返回 0");
                0
            }
        }
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

        // 检查 Agent 状态，禁止并发请求（使用统一 Registry）
        if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(&project_id) {
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

        // 解析 service_type（默认为 RCoder）
        let service_type = req
            .service_type
            .as_ref()
            .and_then(|st| match st.as_str() {
                "ComputerAgentRunner" => Some(shared_types::ServiceType::ComputerAgentRunner),
                "RCoder" => Some(shared_types::ServiceType::RCoder),
                _ => {
                    warn!("⚠️ [gRPC] 无效的 service_type: {}, 使用默认 RCoder", st);
                    None
                }
            })
            .unwrap_or(shared_types::ServiceType::RCoder);

        debug!("🔧 [gRPC] 使用 service_type: {:?}", service_type);

        // 获取或创建项目工作目录（根据 service_type 使用不同路径）
        let project_dir = match service_type {
            shared_types::ServiceType::ComputerAgentRunner => {
                // ComputerAgentRunner 模式：/home/user/{project_id}
                // 注意：/home/user 是宿主机 computer-project-workspace/{user_id} 的挂载点
                let workspace_path = std::path::PathBuf::from("/home/user").join(&project_id);

                info!(
                    "📁 [gRPC] ComputerAgentRunner 工作目录: {:?}",
                    workspace_path
                );

                workspace_path
            }
            shared_types::ServiceType::RCoder => {
                // RCoder 模式：./project_workspace/{project_id}
                let workspace_path =
                    std::path::PathBuf::from("./project_workspace").join(&project_id);

                info!("📁 [gRPC] RCoder 工作目录: {:?}", workspace_path);

                workspace_path
            }
        };

        // 确保目录存在
        if !project_dir.exists() {
            tokio::fs::create_dir_all(&project_dir)
                .await
                .map_err(|e| Status::internal(format!("创建项目目录失败: {}", e)))?;
        }

        // 生成 request_id
        let request_id = req
            .request_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string().replace("-", ""));

        // 转换新增的配置字段 (v2)
        let agent_config_override = req.agent_config.map(convert_agent_config);

        // 构建 ChatPrompt（包含新增的 override 字段）
        let chat_prompt = ChatPromptBuilder::default()
            .project_id(project_id.clone())
            .project_path(project_dir)
            .session_id(session_id.clone())
            .prompt(req.prompt)
            .attachments(convert_attachments(req.attachments))
            .data_source_attachments(req.data_source_attachments)
            .service_type(service_type) // ✅ 使用从请求中解析的 service_type
            .request_id(request_id.clone())
            // 新增字段 (v2)
            .system_prompt_override(req.system_prompt)
            .user_prompt_template_override(req.user_prompt)
            .agent_config_override(agent_config_override)
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
                    error_code: if response.code != shared_types::error_codes::SUCCESS {
                        Some(response.code)
                    } else {
                        None
                    },
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
            "🛑 [gRPC] CancelSession: session_id={}, project_id={}, reason={}",
            req.session_id, req.project_id, req.reason
        );

        // 🔧 确定实际的 session_id
        // 当 session_id 为空时，根据 project_id 查找
        let actual_session_id = if req.session_id.is_empty() {
            info!(
                "📝 [gRPC] session_id 为空，根据 project_id={} 查找",
                req.project_id
            );

            match AGENT_REGISTRY.get_agent_info(&req.project_id) {
                Some(info) => {
                    let sid = info.session_id.to_string();
                    info!(
                        "✅ [gRPC] 从 project_id={} 获取到 session_id={}",
                        req.project_id, sid
                    );
                    sid
                }
                None => {
                    info!(
                        "ℹ️ [gRPC] project_id={} 无活动会话，取消目标已达成",
                        req.project_id
                    );
                    return Ok(Response::new(CancelResponse {
                        success: true,
                        result: CancelResultType::CancelResultSuccess as i32,
                        message: Some("项目无活动会话".to_string()),
                    }));
                }
            }
        } else {
            req.session_id.clone()
        };

        // 1. 通过统一 Registry 的 O(1) 反向查询获取 project_id
        let project_id = match AGENT_REGISTRY.get_project_by_session(&actual_session_id) {
            Some(pid) => {
                debug!(
                    "✅ [gRPC] 找到 session_id={} 对应的 project_id={}",
                    actual_session_id, pid
                );
                pid
            }
            None => {
                warn!(
                    "⚠️ [gRPC] 未找到 session_id={} 对应的 project",
                    actual_session_id
                );
                // 会话不存在或已完成，返回成功（幂等设计）
                return Ok(Response::new(CancelResponse {
                    success: true,
                    result: CancelResultType::CancelResultSuccess as i32,
                    message: Some("会话不存在或已完成".to_string()),
                }));
            }
        };

        // 2. 获取 agent_info
        let agent_info = match AGENT_REGISTRY.get_agent_info(&project_id) {
            Some(info) => info,
            None => {
                return Ok(Response::new(CancelResponse {
                    success: true,
                    result: CancelResultType::CancelResultSuccess as i32,
                    message: Some("Agent 已停止".to_string()),
                }));
            }
        };

        // 3. 创建 SessionId 和 CancelNotification
        let session_id_obj = SessionId::new(Arc::from(actual_session_id.as_str()));
        let cancel_notification = CancelNotification::new(session_id_obj);

        // 4. 创建 oneshot channel 等待取消结果
        let (result_tx, result_rx) = oneshot::channel::<CancelResult>();
        let cancel_request = CancelNotificationRequestWrapper {
            cancel_notification,
            result_tx,
        };

        // 5. 发送取消通知
        if let Err(e) = agent_info.cancel_tx.send(cancel_request) {
            error!("❌ [gRPC] 发送取消通知失败: {}", e);
            return Ok(Response::new(CancelResponse {
                success: false,
                result: CancelResultType::CancelResultFailed as i32,
                message: Some(format!("发送取消通知失败: {}", e)),
            }));
        }

        info!(
            "📡 [gRPC] 等待 Agent 取消响应: session_id={}",
            actual_session_id
        );

        // 6. 等待取消响应（带超时）
        match tokio::time::timeout(Duration::from_secs(30), result_rx).await {
            Ok(Ok(cancel_result)) => {
                let is_success = cancel_result.is_success();
                info!(
                    "✅ [gRPC] 收到 Agent 取消响应: session_id={}, success={}",
                    actual_session_id, is_success
                );

                if is_success {
                    // 清理 SESSION_CACHE
                    if let Some(session_data) = SESSION_CACHE.get(&actual_session_id) {
                        session_data.close_current_connection();
                    }
                    if SESSION_CACHE.remove(&actual_session_id).is_some() {
                        info!(
                            "🗑️ [gRPC] 已清理 SESSION_CACHE: session_id={}",
                            actual_session_id
                        );
                    }

                    Ok(Response::new(CancelResponse {
                        success: true,
                        result: CancelResultType::CancelResultSuccess as i32,
                        message: Some("取消成功".to_string()),
                    }))
                } else {
                    Ok(Response::new(CancelResponse {
                        success: false,
                        result: CancelResultType::CancelResultFailed as i32,
                        message: Some("Agent 取消执行失败".to_string()),
                    }))
                }
            }
            Ok(Err(e)) => {
                error!(
                    "❌ [gRPC] 等待 Agent 取消响应通道关闭: session_id={}, error={:?}",
                    actual_session_id, e
                );
                Ok(Response::new(CancelResponse {
                    success: false,
                    result: CancelResultType::CancelResultFailed as i32,
                    message: Some(format!("响应通道关闭: {}", e)),
                }))
            }
            Err(_) => {
                warn!(
                    "⚠️ [gRPC] 等待 Agent 取消响应超时: session_id={}",
                    actual_session_id
                );
                Ok(Response::new(CancelResponse {
                    success: false,
                    result: CancelResultType::CancelResultTimeout as i32,
                    message: Some("取消请求超时（30秒）".to_string()),
                }))
            }
        }
    }

    /// 获取 Agent 状态
    #[instrument(skip(self, request))]
    async fn get_status(
        &self,
        request: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        let req = request.into_inner();
        info!("📊 [gRPC] GetStatus: project_id={}", req.project_id);

        // 使用统一 Registry 获取 Agent 状态
        let status = if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(&req.project_id) {
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

    /// 停止 Agent（用于 ComputerAgentRunner 模式）
    ///
    /// 停止指定项目的 Agent，但不销毁容器。
    /// 与 cancel_session 的区别：
    /// - cancel_session: 取消单个会话任务
    /// - stop_agent: 停止整个 Agent 进程（可能有多个会话）
    #[instrument(skip(self, request))]
    async fn stop_agent(
        &self,
        request: Request<shared_types::grpc::StopAgentRequest>,
    ) -> Result<Response<shared_types::grpc::StopAgentResponse>, Status> {
        use shared_types::grpc::StopAgentResponse;

        let req = request.into_inner();
        let project_id = req.project_id.clone();
        let force = req.force;
        let reason = req
            .reason
            .clone()
            .unwrap_or_else(|| "用户请求停止".to_string());

        info!(
            "🛑 [gRPC] StopAgent: project_id={}, force={}, reason={}",
            project_id, force, reason
        );

        // 检查 Agent 是否存在
        let agent_info = match AGENT_REGISTRY.get_agent_info(&project_id) {
            Some(info) => info,
            None => {
                info!("📭 [gRPC] Agent 不存在: project_id={}", project_id);
                return Ok(Response::new(StopAgentResponse {
                    success: true,
                    result: "not_found".to_string(),
                    message: Some(format!("项目 {} 的 Agent 不存在或已停止", project_id)),
                    project_id,
                }));
            }
        };

        // 如果 Agent 已经在 Terminating 状态，返回 already_stopped
        if agent_info.status == AgentStatus::Terminating {
            info!("ℹ️ [gRPC] Agent 已经在停止中: project_id={}", project_id);
            return Ok(Response::new(StopAgentResponse {
                success: true,
                result: "already_stopped".to_string(),
                message: Some(format!("项目 {} 的 Agent 已在停止中", project_id)),
                project_id,
            }));
        }

        // 获取当前 session_id（如果有活动会话）
        let session_id = agent_info.session_id.to_string();

        // 如果 force=true 或者 Agent 处于 Idle 状态，直接停止
        if force || agent_info.status == AgentStatus::Idle {
            info!(
                "🔥 [gRPC] 强制停止或 Idle 状态，直接清理: project_id={}",
                project_id
            );

            // 清理 SESSION_CACHE
            if !session_id.is_empty() {
                if let Some(session_data) = SESSION_CACHE.get(&session_id) {
                    session_data.close_current_connection();
                }
                if SESSION_CACHE.remove(&session_id).is_some() {
                    info!("🗑️ [gRPC] 已清理 SESSION_CACHE: session_id={}", session_id);
                }
            }

            // 从 AGENT_REGISTRY 移除（触发 AgentLifecycleGuard drop）
            if AGENT_REGISTRY.remove_by_project(&project_id).is_some() {
                info!("✅ [gRPC] Agent 已停止: project_id={}", project_id);
                return Ok(Response::new(StopAgentResponse {
                    success: true,
                    result: "stopped".to_string(),
                    message: Some(format!("项目 {} 的 Agent 已成功停止", project_id)),
                    project_id,
                }));
            } else {
                warn!(
                    "⚠️ [gRPC] 从 Registry 移除 Agent 失败: project_id={}",
                    project_id
                );
                return Ok(Response::new(StopAgentResponse {
                    success: false,
                    result: "error".to_string(),
                    message: Some("移除 Agent 失败".to_string()),
                    project_id,
                }));
            }
        }

        // 如果 force=false 且 Agent 正在执行任务（Active），需要先取消会话
        if agent_info.status == AgentStatus::Active {
            info!(
                "📡 [gRPC] Agent 正在执行任务，先取消会话: project_id={}, session_id={}",
                project_id, session_id
            );

            // 创建 SessionId 和 CancelNotification
            let session_id_obj = SessionId::new(Arc::from(session_id.as_str()));
            let cancel_notification = CancelNotification::new(session_id_obj);

            // 创建 oneshot channel 等待取消结果
            let (result_tx, result_rx) = oneshot::channel::<CancelResult>();
            let cancel_request = CancelNotificationRequestWrapper {
                cancel_notification,
                result_tx,
            };

            // 发送取消通知
            if let Err(e) = agent_info.cancel_tx.send(cancel_request) {
                error!(
                    "❌ [gRPC] 发送取消通知失败: project_id={}, error={}",
                    project_id, e
                );
                return Ok(Response::new(StopAgentResponse {
                    success: false,
                    result: "error".to_string(),
                    message: Some(format!("发送取消通知失败: {}", e)),
                    project_id,
                }));
            }

            // 等待取消响应（带超时）
            match tokio::time::timeout(Duration::from_secs(30), result_rx).await {
                Ok(Ok(cancel_result)) => {
                    if cancel_result.is_success() {
                        info!(
                            "✅ [gRPC] 会话取消成功，继续停止 Agent: project_id={}",
                            project_id
                        );

                        // 清理 SESSION_CACHE
                        if let Some(session_data) = SESSION_CACHE.get(&session_id) {
                            session_data.close_current_connection();
                        }
                        if SESSION_CACHE.remove(&session_id).is_some() {
                            info!("🗑️ [gRPC] 已清理 SESSION_CACHE: session_id={}", session_id);
                        }

                        // 从 AGENT_REGISTRY 移除
                        if AGENT_REGISTRY.remove_by_project(&project_id).is_some() {
                            info!("✅ [gRPC] Agent 已停止: project_id={}", project_id);
                            return Ok(Response::new(StopAgentResponse {
                                success: true,
                                result: "stopped".to_string(),
                                message: Some(format!("项目 {} 的 Agent 已成功停止", project_id)),
                                project_id,
                            }));
                        } else {
                            warn!(
                                "⚠️ [gRPC] 从 Registry 移除 Agent 失败: project_id={}",
                                project_id
                            );
                            return Ok(Response::new(StopAgentResponse {
                                success: false,
                                result: "error".to_string(),
                                message: Some("移除 Agent 失败".to_string()),
                                project_id,
                            }));
                        }
                    } else {
                        warn!(
                            "⚠️ [gRPC] 会话取消失败，Agent 停止失败: project_id={}",
                            project_id
                        );
                        return Ok(Response::new(StopAgentResponse {
                            success: false,
                            result: "error".to_string(),
                            message: Some("取消会话失败".to_string()),
                            project_id,
                        }));
                    }
                }
                Ok(Err(e)) => {
                    error!(
                        "❌ [gRPC] 等待取消响应通道关闭: project_id={}, error={:?}",
                        project_id, e
                    );
                    return Ok(Response::new(StopAgentResponse {
                        success: false,
                        result: "error".to_string(),
                        message: Some(format!("响应通道关闭: {}", e)),
                        project_id,
                    }));
                }
                Err(_) => {
                    warn!("⏰ [gRPC] 等待取消响应超时: project_id={}", project_id);
                    return Ok(Response::new(StopAgentResponse {
                        success: false,
                        result: "error".to_string(),
                        message: Some("取消请求超时（30秒）".to_string()),
                        project_id,
                    }));
                }
            }
        }

        // 理论上不会走到这里
        warn!(
            "⚠️ [gRPC] StopAgent 走到了意外分支: project_id={}",
            project_id
        );
        Ok(Response::new(StopAgentResponse {
            success: false,
            result: "error".to_string(),
            message: Some("意外的代码分支".to_string()),
            project_id,
        }))
    }

    /// 查询容器状态（用于容器生命周期管理）
    ///
    /// 返回容器的活跃状态、活跃任务数、运行时长等信息。
    /// RCoder 定期调用此接口来判断容器是否应该被保活。
    #[instrument(skip(self))]
    async fn get_container_status(
        &self,
        request: Request<GetContainerStatusRequest>,
    ) -> Result<Response<GetContainerStatusResponse>, Status> {
        let req = request.into_inner();

        info!(
            "🔍 [GET_CONTAINER_STATUS] 收到容器状态查询: user_id={}, project_id={}",
            req.user_id, req.project_id
        );

        // 查询当前活跃任务数
        let active_tasks = self.get_active_tasks_count();

        // 计算容器运行时长（秒）
        let uptime_seconds = self.get_uptime_seconds();

        // 判断容器是否活跃：有活跃任务则认为容器活跃
        let is_active = active_tasks > 0;

        // 状态描述
        let status = if active_tasks > 0 {
            "Processing".to_string()
        } else {
            "Idle".to_string()
        };

        let response = GetContainerStatusResponse {
            is_active,
            active_tasks,
            uptime_seconds,
            status: status.clone(),
            cpu_percent: None, // 可选，未来实现
            memory_mb: None,   // 可选，未来实现
        };

        debug!(
            "✅ [GET_CONTAINER_STATUS] 返回容器状态: is_active={}, active_tasks={}, status={}, uptime={}s",
            response.is_active, response.active_tasks, response.status, response.uptime_seconds
        );

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
        request_id: message
            .data
            .get("request_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        timestamp,
    }
}
