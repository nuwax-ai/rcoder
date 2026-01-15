//! AgentService gRPC 服务实现
//!
//! 实现 `agent.AgentService` 定义的所有 RPC 方法

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use sacp::schema::{CancelNotification, SessionId};
use shared_types::ModelProviderConfig;
use shared_types::grpc::{
    CancelRequest, CancelResponse, CancelResultType, ChatAgentConfig as GrpcChatAgentConfig,
    ChatAgentServerConfig as GrpcChatAgentServerConfig,
    ChatContextServerConfig as GrpcChatContextServerConfig, ChatRequest as GrpcChatRequest,
    ChatResponse as GrpcChatResponse, GetContainerStatusRequest, GetContainerStatusResponse,
    GetStatusRequest, GetStatusResponse, GetVncStatusRequest, GetVncStatusResponse,
    ModelProviderConfig as GrpcModelProviderConfig, ProgressEvent, ProgressRequest,
    agent_service_server::AgentService, attachment, attachment_source,
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
use crate::proxy_agent::AgentRequest;
use crate::router::AppState;
use crate::service::{AGENT_REGISTRY, SESSION_CACHE};
use crate::{CancelNotificationRequestWrapper, CancelResult};
use dashmap::DashSet;
use shared_types::ChatPromptBuilder;
use std::sync::LazyLock;

/// 🆕 全局 Session 首次 Prompt 成功标记
///
/// 用于跟踪哪些 session 已经成功执行过至少一次 Prompt
/// - 如果 session_id 在集合中 → 已成功执行过
/// - 如果不在集合中 → 未执行过（首次）
///
/// 使用 DashSet 比 DashMap<String, bool> 更轻量，因为只需要判断存在性
pub(crate) static SESSION_FIRST_PROMPT_SUCCESS: LazyLock<DashSet<String>> =
    LazyLock::new(|| DashSet::new());

/// 将 gRPC ModelProviderConfig 转换为内部 ModelProviderConfig
fn convert_model_provider(grpc_config: GrpcModelProviderConfig) -> ModelProviderConfig {
    ModelProviderConfig {
        id: grpc_config.id, // 保留原始 ID，用于会话复用判断
        name: grpc_config.provider,
        base_url: grpc_config.api_base.unwrap_or_default(),
        api_key: grpc_config.api_key.unwrap_or_default(),
        requires_openai_auth: grpc_config.requires_openai_auth.unwrap_or(true),
        default_model: grpc_config.model,
        api_protocol: grpc_config.api_protocol,
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

        // 使用脱敏包装器格式化 model_config
        let model_config_debug = req
            .model_config
            .as_ref()
            .map(|cfg| shared_types::MaskedModelConfig(cfg));

        info!(
            "🚀 [gRPC] Chat 请求: project_id={}, session_id={}, prompt_len={}, model_config={:?}, service_type={:?}, user_id={:?}, has_attachments={}, has_data_source={}",
            req.project_id,
            req.session_id,
            req.prompt.len(),
            model_config_debug,
            req.service_type,
            req.user_id,
            !req.attachments.is_empty(),
            !req.data_source_attachments.is_empty()
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
        // 🆕 检查 Active 和 Pending 两种状态
        if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(&project_id) {
            if agent_info.status == AgentStatus::Active || agent_info.status == AgentStatus::Pending
            {
                // 🎯 使用业务响应返回错误码，而非 gRPC Status 错误
                // 这样 rcoder 可以直接从 error_code 字段读取错误码
                info!(
                    "🚫 [gRPC] Agent Busy 返回 9010 错误: project_id={}, status={:?}",
                    project_id, agent_info.status
                );
                return Ok(Response::new(GrpcChatResponse {
                    project_id: project_id.clone(),
                    session_id: session_id.unwrap_or_default(),
                    success: false,
                    error: Some("Agent正在执行任务，请等待当前任务完成后再发送新请求".to_string()),
                    error_code: Some(shared_types::error_codes::ERR_AGENT_BUSY.to_string()),
                    request_id: req.request_id.clone(),
                    // 🆕 Agent Busy 不需要降级
                    need_fallback: false,
                    fallback_reason: None,
                }));
            }
        }

        // 🆕 预注册：在发送任务到队列前，立即将 project 标记为 Pending
        // 这样并发请求会被上面的忙碌检查拦截
        AGENT_REGISTRY.set_pending(&project_id);
        info!("📌 [gRPC] 预注册 Pending 状态: project_id={}", project_id);

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

        // 🔥 生成唯一的 service UUID（用于 API 密钥管理）
        let service_uuid = if model_provider.is_some() {
            Some(uuid::Uuid::new_v4().to_string())
        } else {
            None
        };

        if let Some(ref provider) = model_provider {
            let service_uuid_ref = service_uuid.as_ref().unwrap();

            debug!(
                "📝 [gRPC] 使用模型配置: provider={}, model={}, base_url={}, api_protocol={:?}, requires_openai_auth={}, service_uuid={}",
                provider.name,
                provider.default_model,
                provider.base_url,
                provider.api_protocol,
                provider.requires_openai_auth,
                service_uuid_ref
            );

            // 🔒 存储 ModelProviderConfig 到共享 DashMap（使用 UUID 作为 key）
            // key: UUID, value: ModelProviderConfig
            self.app_state
                .shared_api_key_manager
                .insert(service_uuid_ref.clone(), provider.clone());

            // 🔒 存储 project_id -> UUID 映射（用于后续清理时查找）
            // 使用独立的 DashMap，类型清晰，key 使用 project_id 便于清理时查找
            self.app_state
                .project_uuid_map
                .insert(project_id.clone(), service_uuid_ref.clone());

            // ✅ ApiKeyManager 现在是 shared_api_key_manager 的包装器，不需要单独写入

            info!(
                "🔑 [gRPC] 已存储 API 配置: service_uuid={}, provider_name={}, base_url={}",
                service_uuid_ref,
                provider.name,
                shared_types::mask_url(&provider.base_url)
            );
        } else {
            warn!("⚠️ [gRPC] 未提供模型配置，将使用环境变量或默认配置");
        }

        // 创建请求并设置 UUID 和密钥管理器
        let (local_task_request, chat_prompt_rx) =
            AgentRequest::new(prompt_message, model_provider);
        let local_task_request = local_task_request
            .with_service_uuid(service_uuid)
            .with_key_manager(Some(self.app_state.shared_api_key_manager.clone()));

        // 🆕 检查 worker 状态
        use crate::agent_worker_manager::WorkerState;
        match self.app_state.agent_worker_manager.state() {
            WorkerState::Running => {
                // 正常运行，继续处理
            }
            WorkerState::Starting => {
                warn!("⚠️ [gRPC] Agent Worker 正在启动，请求可能被延迟处理");
            }
            WorkerState::Stopping | WorkerState::Stopped => {
                // 🔥 关键修复：清理 Pending 状态
                AGENT_REGISTRY.clear_pending_if_exists(&project_id);
                // Worker 不可用
                return Err(Status::unavailable(
                    "Agent Worker 不可用，正在重启中。请稍后重试",
                ));
            }
        }

        // 🆕 使用 manager 发送（带状态检查）
        if let Err(e) = self
            .app_state
            .agent_worker_manager
            .try_send(local_task_request)
            .await
        {
            // 🔥 关键修复：发送失败时清理 Pending 状态
            AGENT_REGISTRY.clear_pending_if_exists(&project_id);
            return Err(Status::internal(format!("发送任务失败: {}", e)));
        }

        // 等待响应
        match chat_prompt_rx.await {
            Ok(response) => {
                // 🆕 检测是否需要降级（Resume 失败）
                // 判断逻辑：
                // 1. 是 Resume 会话（session_id 不为空）
                // 2. Prompt 执行失败（有错误）
                // 3. 是首次 Prompt（SESSION_FIRST_PROMPT_SUCCESS 中没有该 session_id）
                let is_resume_session = session_id.is_some();
                let has_error = response.error.is_some();

                let (need_fallback, fallback_reason) = if is_resume_session && has_error {
                    let session_id_str = session_id.as_ref().unwrap();
                    let is_first_prompt = !SESSION_FIRST_PROMPT_SUCCESS.contains(session_id_str);

                    if is_first_prompt {
                        warn!(
                            "⚠️ [gRPC] 检测到 Resume 会话首次 Prompt 失败，需要降级: project_id={}, session_id={}, error={}",
                            response.project_id,
                            session_id_str,
                            response.error.as_ref().unwrap()
                        );
                        (true, Some("resume_first_prompt_failed".to_string()))
                    } else {
                        // 不是首次 Prompt，正常错误处理
                        (false, None)
                    }
                } else {
                    (false, None)
                };

                // 🆕 如果 Prompt 执行成功，标记该 session 已成功执行过 Prompt
                if !has_error {
                    if let Some(ref session_id_str) = session_id {
                        SESSION_FIRST_PROMPT_SUCCESS.insert(session_id_str.clone());
                        debug!(
                            "✅ [gRPC] 标记 session 首次 Prompt 成功: session_id={}",
                            session_id_str
                        );
                    }
                }

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
                    // 🆕 添加降级标识字段
                    need_fallback,
                    fallback_reason,
                };

                if need_fallback {
                    info!(
                        "✅ [gRPC] Chat 完成 (需要降级): success={}, need_fallback={}",
                        grpc_response.success, grpc_response.need_fallback
                    );
                } else {
                    info!("✅ [gRPC] Chat 完成: success={}", grpc_response.success);
                }

                Ok(Response::new(grpc_response))
            }
            Err(e) => {
                error!("❌ [gRPC] Chat 失败: {}", e);
                // 🆕 清理 Pending 状态，避免死锁
                AGENT_REGISTRY.clear_pending_if_exists(&project_id);
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
                                info!("📡 [gRPC] Session 连接被取消，发送 SessionPromptEnd: session_id={}", session_id_clone);

                                // ✅ 在断开连接之前，主动发送 SessionPromptEnd 消息
                                use sacp::schema::StopReason;
                                use shared_types::{SessionNotify, SessionPromptEnd};

                                let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
                                    session_id: session_id_clone.clone(),
                                    stop_reason: StopReason::Cancelled,
                                    error_message: Some("用户主动取消任务".to_string()),
                                    request_id: None,
                                });
                                let unified_message = notify.to_unified_message();
                                let end_event = unified_message_to_progress_event(&unified_message);

                                // 发送结束事件（忽略错误，因为客户端可能已经断开）
                                if let Err(e) = tx.send(Ok(end_event)).await {
                                    warn!("📡 [gRPC] 发送 SessionPromptEnd 事件失败: session_id={}, error={}", session_id_clone, e);
                                }

                                break;
                            }
                            msg = message_rx.recv() => {
                                match msg {
                                    Some(unified_message) => {
                                        // 🎯 检查是否为终止消息（SessionPromptEnd）
                                        let is_terminal_message = matches!(
                                            unified_message.message_type,
                                            crate::model::SessionMessageType::SessionPromptEnd
                                        );

                                        let event = unified_message_to_progress_event(&unified_message);
                                        if tx.send(Ok(event)).await.is_err() {
                                            debug!("📡 [gRPC] 客户端已断开连接");
                                            break;
                                        }

                                        // 🚀 收到终止消息后，主动关闭 gRPC 流
                                        // 不再等待 channel 关闭或心跳超时
                                        if is_terminal_message {
                                            info!(
                                                "🔚 [gRPC] 收到 SessionPromptEnd，主动关闭流: session_id={}, sub_type={}",
                                                session_id_clone, unified_message.sub_type
                                            );
                                            break;
                                        }
                                    }
                                    None => {
                                        debug!("📡 [gRPC] Session 消息通道已关闭，发送 SessionPromptEnd 事件");
                                        // Agent 执行完毕，发送 SessionPromptEnd 事件通知客户端（兜底逻辑）
                                        let end_event = ProgressEvent {
                                            message_type: "SessionPromptEnd".to_string(),
                                            sub_type: "end_turn".to_string(),
                                            payload: r#"{"reason":"EndTurn","description":"Agent 当前无在执行任务"}"#.to_string(),
                                            request_id: None,
                                            timestamp: chrono::Utc::now().timestamp_millis(),
                                        };
                                        if let Err(e) = tx.send(Ok(end_event)).await {
                                            warn!("📡 [gRPC] 发送 SessionPromptEnd 事件失败: session_id={}, error={}", session_id_clone, e);
                                        }
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
                    if let Err(send_err) = tx
                        .send(Err(Status::internal(format!("创建连接失败: {}", e))))
                        .await
                    {
                        warn!(
                            "📡 [gRPC] 发送错误状态失败: session_id={}, error={}",
                            session_id_clone, send_err
                        );
                    }
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

        // 2. 获取 agent_info 并提取需要的数据
        // 🔥 关键修复：使用代码块限制读锁生命周期，避免在 .await 时持有读锁导致死锁
        let (status, cancel_tx) = {
            let agent_info = match AGENT_REGISTRY.get_agent_info(&project_id) {
                Some(info) => info,
                None => {
                    info!(
                        "ℹ️ [gRPC] project_id={} 无活动会话，取消目标已达成（幂等）",
                        project_id
                    );
                    return Ok(Response::new(CancelResponse {
                        success: true,
                        result: CancelResultType::CancelResultSuccess as i32,
                        message: Some("项目无活动会话".to_string()),
                    }));
                }
            };

            // ✅ 主动 clone 数据，然后显式 drop 读锁
            // status 是 Copy 类型，直接复制
            // cancel_tx 是 UnboundedSender，clone 成本很低（内部只是 Arc 引用计数+1）
            let status = agent_info.status;
            let cancel_tx = agent_info.cancel_tx.clone();

            // 🔥 显式释放读锁，确保在代码块结束前就释放
            drop(agent_info);

            (status, cancel_tx)
        };

        // 🆕 ===== 幂等性检查与通道有效性验证 =====
        match status {
            AgentStatus::Idle => {
                // 已经是 Idle 状态，无需取消（幂等返回）
                info!(
                    "✅ [gRPC] Agent 已经是 Idle 状态，取消请求幂等成功: project_id={}, session_id={}",
                    project_id, actual_session_id
                );
                return Ok(Response::new(CancelResponse {
                    success: true,
                    result: CancelResultType::CancelResultSuccess as i32,
                    message: Some("Agent 已经是空闲状态".to_string()),
                }));
            }
            AgentStatus::Terminating => {
                // 正在停止中，无需重复取消（幂等返回）
                info!(
                    "✅ [gRPC] Agent 正在停止中，取消请求幂等成功: project_id={}, session_id={}",
                    project_id, actual_session_id
                );
                return Ok(Response::new(CancelResponse {
                    success: true,
                    result: CancelResultType::CancelResultSuccess as i32,
                    message: Some("Agent 正在停止中".to_string()),
                }));
            }
            AgentStatus::Active | AgentStatus::Pending => {
                // 正常流程：继续执行取消
                debug!(
                    "🔄 [gRPC] Agent 状态为 {:?}，执行取消操作: project_id={}, session_id={}",
                    status, project_id, actual_session_id
                );
            }
        }

        // 🆕 验证 cancel_tx 通道是否仍然有效
        // 避免：LocalSet 已退出导致 cancel_tx 失效，但 send 时才发现的问题
        if cancel_tx.is_closed() {
            error!(
                "❌ [gRPC] cancel_tx 通道已关闭，LocalSet 可能已意外退出: project_id={}, session_id={}",
                project_id, actual_session_id
            );
            return Ok(Response::new(CancelResponse {
                success: false,
                result: CancelResultType::CancelResultFailed as i32,
                message: Some("取消通道已关闭，Agent 可能已停止".to_string()),
            }));
        }

        // 3. 创建 SessionId 和 CancelNotification
        let session_id_obj = SessionId::new(Arc::from(actual_session_id.as_str()));
        let cancel_notification = CancelNotification::new(session_id_obj);

        // 4. 创建 oneshot channel 等待取消结果
        let (result_tx, result_rx) = oneshot::channel::<CancelResult>();
        let cancel_request = CancelNotificationRequestWrapper {
            cancel_notification,
            result_tx,
        };

        // 5. 发送取消通知（使用已提取的 cancel_tx）
        if let Err(e) = cancel_tx.send(cancel_request) {
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
                    // 🆕 主动发送 SessionPromptEnd 消息通知 SSE 客户端任务已取消
                    use crate::service::push_session_update_with_project;
                    use sacp::schema::StopReason;
                    use shared_types::{SessionNotify, SessionPromptEnd};

                    let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
                        session_id: actual_session_id.clone(),
                        stop_reason: StopReason::Cancelled,
                        error_message: None,
                        request_id: None,
                    });

                    if let Err(e) =
                        push_session_update_with_project(&project_id, &actual_session_id, notify)
                            .await
                    {
                        warn!("⚠️ [gRPC] 发送 SessionPromptEnd 通知失败: {}", e);
                    } else {
                        info!(
                            "📤 [gRPC] 已发送 SessionPromptEnd (Cancelled) 通知: session_id={}",
                            actual_session_id
                        );
                    }

                    // 🔥 关键修复：先 clone 数据，释放读锁，再调用 .await
                    // 避免：持有 SESSION_CACHE 读锁时调用 .await 导致死锁
                    if let Some(session_data_ref) = SESSION_CACHE.get(&actual_session_id) {
                        let session_data = session_data_ref.clone();
                        drop(session_data_ref); // 显式释放读锁
                        session_data.close_current_connection().await; // ✅ 安全：已无读锁
                    }
                    if SESSION_CACHE.remove(&actual_session_id).is_some() {
                        info!(
                            "🗑️ [gRPC] 已清理 SESSION_CACHE: session_id={}",
                            actual_session_id
                        );
                    }

                    // 🔥 关键修复：清理 AGENT_REGISTRY，将状态更新为 Idle
                    // 使用原子性操作避免竞态条件
                    use chrono::Utc;
                    use shared_types::AgentStatus;

                    let updated = AGENT_REGISTRY.try_update_agent_info(&project_id, |info| {
                        // 只在 Active/Pending 状态时更新
                        if matches!(info.status, AgentStatus::Active | AgentStatus::Pending) {
                            info.status = AgentStatus::Idle;
                            info.last_activity = Utc::now();
                            true
                        } else {
                            false
                        }
                    });

                    if updated {
                        info!(
                            "✅ [gRPC] Agent 状态已原子性更新为 Idle: project_id={}, session_id={}",
                            project_id, actual_session_id
                        );
                    } else {
                        debug!(
                            "🔍 [gRPC] Agent 状态无需更新（可能已是 Idle 或其他状态）: project_id={}, session_id={}",
                            project_id, actual_session_id
                        );
                    }

                    Ok(Response::new(CancelResponse {
                        success: true,
                        result: CancelResultType::CancelResultSuccess as i32,
                        message: Some("取消成功".to_string()),
                    }))
                } else {
                    // 取消失败，Agent 可能还在运行，不发送 SessionPromptEnd
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

                // 通道关闭说明 Agent 处理线程已崩溃或退出，发送 SessionPromptEnd
                use crate::service::push_session_update_with_project;
                use sacp::schema::StopReason;
                use shared_types::{SessionNotify, SessionPromptEnd};

                let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
                    session_id: actual_session_id.clone(),
                    stop_reason: StopReason::Cancelled,
                    error_message: Some(format!("Agent 响应通道关闭: {}", e)),
                    request_id: None,
                });

                if let Err(notify_err) =
                    push_session_update_with_project(&project_id, &actual_session_id, notify).await
                {
                    warn!("⚠️ [gRPC] 发送 SessionPromptEnd 通知失败: {}", notify_err);
                }

                // 清理连接
                // 🔥 关键修复：先 clone 数据，释放读锁，再调用 .await
                if let Some(session_data_ref) = SESSION_CACHE.get(&actual_session_id) {
                    let session_data = session_data_ref.clone();
                    drop(session_data_ref); // 显式释放读锁
                    session_data.close_current_connection().await; // ✅ 安全：已无读锁
                }

                Ok(Response::new(CancelResponse {
                    success: false,
                    result: CancelResultType::CancelResultFailed as i32,
                    message: Some(format!("响应通道关闭: {}", e)),
                }))
            }
            Err(_) => {
                warn!(
                    "⚠️ [gRPC] 等待 Agent 取消响应超时: session_id={}, 主动清理资源",
                    actual_session_id
                );

                // 🆕 超时时主动清理资源，确保一致性
                use crate::service::push_session_update_with_project;
                use sacp::schema::StopReason;
                use shared_types::{SessionNotify, SessionPromptEnd};

                // 1. 发送 SessionPromptEnd 通知
                let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
                    session_id: actual_session_id.clone(),
                    stop_reason: StopReason::Cancelled,
                    error_message: Some("取消请求超时，主动清理资源".to_string()),
                    request_id: None,
                });

                if let Err(e) =
                    push_session_update_with_project(&project_id, &actual_session_id, notify).await
                {
                    warn!("⚠️ [gRPC] 发送 SessionPromptEnd 通知失败: {}", e);
                } else {
                    info!(
                        "📤 [gRPC] 已发送 SessionPromptEnd (Timeout) 通知: session_id={}",
                        actual_session_id
                    );
                }

                // 2. 清理 SESSION_CACHE
                // 🔥 关键修复：先 clone 数据，释放读锁，再调用 .await
                if let Some(session_data_ref) = SESSION_CACHE.get(&actual_session_id) {
                    let session_data = session_data_ref.clone();
                    drop(session_data_ref); // 显式释放读锁
                    session_data.close_current_connection().await; // ✅ 安全：已无读锁
                }
                if SESSION_CACHE.remove(&actual_session_id).is_some() {
                    info!(
                        "🗑️ [gRPC] 已清理 SESSION_CACHE: session_id={}",
                        actual_session_id
                    );
                }

                // 3. 🆕 使用 DashMap entry API 原子性地更新 Agent 状态为 Idle
                // 避免：读锁释放 → 时间窗口 → 写锁更新 的竞态条件
                use chrono::Utc;
                use shared_types::AgentStatus;

                let updated = AGENT_REGISTRY.try_update_agent_info(&project_id, |info| {
                    // 只在 Active 状态时更新，避免覆盖其他状态
                    if info.status == AgentStatus::Active {
                        info.status = AgentStatus::Idle;
                        info.last_activity = Utc::now();
                        true
                    } else {
                        false
                    }
                });

                if updated {
                    info!(
                        "✅ [gRPC] 超时后 Agent 状态已原子性更新为 Idle: project_id={}, session_id={}",
                        project_id, actual_session_id
                    );
                } else {
                    debug!(
                        "🔍 [gRPC] Agent 状态无需更新（非 Active 状态）: project_id={}, session_id={}",
                        project_id, actual_session_id
                    );
                }

                Ok(Response::new(CancelResponse {
                    success: false,
                    result: CancelResultType::CancelResultTimeout as i32,
                    message: Some("取消请求超时（30秒）".to_string()),
                }))
            }
        }
    }

    /// 获取 Agent 状态
    ///
    /// 支持通过 `project_id` 或 `session_id` 查询 Agent 状态
    #[instrument(skip(self, request))]
    async fn get_status(
        &self,
        request: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        let req = request.into_inner();
        info!(
            "📊 [gRPC] GetStatus: project_id={}, session_id={}",
            req.project_id, req.session_id
        );

        // 优先使用 session_id 查询 project_id
        let project_id = if !req.session_id.is_empty() {
            // 通过 session_id 反查 project_id
            AGENT_REGISTRY.get_project_by_session(&req.session_id)
        } else if !req.project_id.is_empty() {
            // 使用提供的 project_id
            Some(req.project_id)
        } else {
            // 两者都为空，返回 idle 且 is_found=false
            info!("📊 [gRPC] GetStatus: 参数都为空，返回 not_found");
            return Ok(Response::new(GetStatusResponse {
                status: "idle".to_string(),
                is_found: false,
            }));
        };

        // 查询 Agent 状态 - 使用 &str 避免重复 to_string()
        let (status_str, is_found) = if let Some(ref pid) = project_id {
            if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(pid) {
                let status_str = match agent_info.status {
                    AgentStatus::Pending => "busy",
                    AgentStatus::Active => "busy",
                    AgentStatus::Idle => "idle",
                    AgentStatus::Terminating => "busy",
                };
                (status_str, true) // Agent 存在
            } else {
                // project_id 不存在，说明 Agent 已完成/未注册
                ("idle", false)
            }
        } else {
            // session_id 没有对应的 project_id，说明 session 不存在或已完成
            ("idle", false)
        };

        info!(
            "📊 [gRPC] GetStatus 结果: status={}, is_found={}, project_id={:?}",
            status_str, is_found, project_id
        );

        // 只在最后需要时才转换为 String
        Ok(Response::new(GetStatusResponse {
            status: status_str.to_string(),
            is_found,
        }))
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

        // 检查 Agent 是否存在，并提取需要的数据后立即释放读锁
        // ⚠️ 重要：必须在任何 .await 之前 drop 掉 Ref，否则会导致死锁
        let (agent_status, session_id, cancel_tx) = match AGENT_REGISTRY.get_agent_info(&project_id)
        {
            Some(info) => {
                let status = info.status.clone();
                let session_id = info.session_id.to_string();
                let cancel_tx = info.cancel_tx.clone();
                // info（Ref）在这里被 drop，释放读锁
                (status, session_id, cancel_tx)
            }
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
        if agent_status == AgentStatus::Terminating {
            info!("ℹ️ [gRPC] Agent 已经在停止中: project_id={}", project_id);

            // 🆕 即使已在停止中，也要发送 SessionPromptEnd 确保前端收到结束消息
            if !session_id.is_empty() {
                use crate::service::push_session_update_with_project;
                use sacp::schema::StopReason;
                use shared_types::{SessionNotify, SessionPromptEnd};

                let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
                    session_id: session_id.clone(),
                    stop_reason: StopReason::Cancelled,
                    error_message: Some("Agent 已在停止中".to_string()),
                    request_id: None,
                });

                if let Err(e) =
                    push_session_update_with_project(&project_id, &session_id, notify).await
                {
                    warn!("⚠️ [gRPC] 发送 SessionPromptEnd 通知失败: {}", e);
                }

                // 清理连接
                // 🔥 关键修复：先 clone 数据，释放读锁，再调用 .await
                if let Some(session_data_ref) = SESSION_CACHE.get(&session_id) {
                    let session_data = session_data_ref.clone();
                    drop(session_data_ref); // 显式释放读锁
                    session_data.close_current_connection().await; // ✅ 安全：已无读锁
                }
            }

            return Ok(Response::new(StopAgentResponse {
                success: true,
                result: "already_stopped".to_string(),
                message: Some(format!("项目 {} 的 Agent 已在停止中", project_id)),
                project_id,
            }));
        }

        // 如果 force=true 或者 Agent 处于 Idle/Pending 状态，直接停止
        // Pending 状态的 Agent 还没有启动 Worker，无法接收取消信号，所以直接清理
        if force || agent_status == AgentStatus::Idle || agent_status == AgentStatus::Pending {
            info!(
                "🔥 [gRPC] 强制停止/Idle/Pending 状态，直接清理: project_id={}, status={:?}",
                project_id, agent_status
            );

            // 🆕 主动发送 SessionPromptEnd 消息通知 SSE 客户端 Agent 已停止
            if !session_id.is_empty() {
                use crate::service::push_session_update_with_project;
                use sacp::schema::StopReason;
                use shared_types::{SessionNotify, SessionPromptEnd};

                let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
                    session_id: session_id.clone(),
                    stop_reason: StopReason::Cancelled,
                    error_message: None,
                    request_id: None,
                });

                if let Err(e) =
                    push_session_update_with_project(&project_id, &session_id, notify).await
                {
                    warn!("⚠️ [gRPC] 发送 SessionPromptEnd 通知失败: {}", e);
                } else {
                    info!(
                        "📤 [gRPC] 已发送 SessionPromptEnd (Cancelled) 通知: session_id={}",
                        session_id
                    );
                }
            }

            // 清理 SESSION_CACHE
            if !session_id.is_empty() {
                // 🔥 关键修复：先 clone 数据，释放读锁，再调用 .await
                if let Some(session_data_ref) = SESSION_CACHE.get(&session_id) {
                    let session_data = session_data_ref.clone();
                    drop(session_data_ref); // 显式释放读锁
                    session_data.close_current_connection().await; // ✅ 安全：已无读锁
                }
                if SESSION_CACHE.remove(&session_id).is_some() {
                    info!("🗑️ [gRPC] 已清理 SESSION_CACHE: session_id={}", session_id);
                }
            }

            // 🔒 清理 API 密钥配置（通过 project_id 查找 uuid）
            // 1. 先从 project_uuid_map 获取 uuid
            if let Some((_, uuid)) = self.app_state.project_uuid_map.remove(&project_id) {
                // 2. 清理 shared_api_key_manager 中的配置
                if let Some((key, config)) = self.app_state.shared_api_key_manager.remove(&uuid) {
                    info!(
                        "🗑️ [gRPC] 已清理 API 密钥配置: uuid={}, provider_name={}",
                        key, config.name
                    );
                }
                info!(
                    "🗑️ [gRPC] 已清理 project UUID 映射: project_id={}, uuid={}",
                    project_id, uuid
                );
            } else {
                debug!(
                    "🔍 [gRPC] 未找到 project UUID 映射: project_id={}",
                    project_id
                );
            }

            // 🎯 使用 remove 原子性地获取 AgentInfo 所有权，避免读锁/写锁竞争
            // 先移除再清理，确保不会有锁竞争问题
            let removed_agent_info = AGENT_REGISTRY.remove_by_project(&project_id);

            if let Some(agent_info) = removed_agent_info {
                info!(
                    "✅ [gRPC] Agent 已从 Registry 移除: project_id={}",
                    project_id
                );

                // 获取 stop_handle 并在后台执行清理
                if let Some(ref stop_handle) = agent_info.stop_handle {
                    info!("🔪 [gRPC] 主动停止 Agent 子进程: project_id={}", project_id);

                    let stop_handle_clone = Arc::clone(stop_handle);
                    let pid_clone = project_id.clone();

                    // 在后台执行 graceful_stop 和资源清理
                    tokio::spawn(async move {
                        // 先停止子进程
                        if let Err(e) = stop_handle_clone.graceful_stop().await {
                            warn!(
                                "⚠️ [gRPC] graceful_stop 失败: {}, project_id={}",
                                e, pid_clone
                            );
                        } else {
                            info!("✅ [gRPC] Agent 子进程已停止: project_id={}", pid_clone);
                        }

                        // 异步 drop AgentInfo（让它在后台慢慢 drop）
                        tokio::task::spawn_blocking(move || {
                            drop(agent_info);
                            info!(
                                "🧹 [gRPC] Agent 资源已彻底清理完成: project_id={}",
                                pid_clone
                            );
                        });
                    });
                } else {
                    // 没有 stop_handle，直接在后台 drop
                    let pid_clone = project_id.clone();
                    tokio::spawn(async move {
                        tokio::task::spawn_blocking(move || {
                            drop(agent_info);
                            info!(
                                "🧹 [gRPC] Agent 资源已彻底清理完成: project_id={}",
                                pid_clone
                            );
                        });
                    });
                }
            } else {
                warn!(
                    "⚠️ [gRPC] Agent 不在 Registry 中: project_id={}",
                    project_id
                );
            }

            // 🚀 立即返回成功响应,不等待后台清理完成
            info!(
                "✅ [gRPC] StopAgent 立即返回成功,后台清理中: project_id={}",
                project_id
            );
            let response_message = format!("项目 {} 的 Agent 正在停止（后台清理中）", project_id);

            return Ok(Response::new(StopAgentResponse {
                success: true,
                result: "stopped".to_string(),
                message: Some(response_message),
                project_id,
            }));
        }

        // 如果 force=false 且 Agent 正在执行任务（Active），需要先取消会话
        if agent_status == AgentStatus::Active {
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

            // 发送取消通知（使用之前提取的 cancel_tx）
            if let Err(e) = cancel_tx.send(cancel_request) {
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

                        // 🆕 主动发送 SessionPromptEnd 消息通知 SSE 客户端 Agent 已停止
                        {
                            use crate::service::push_session_update_with_project;
                            use sacp::schema::StopReason;
                            use shared_types::{SessionNotify, SessionPromptEnd};

                            let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
                                session_id: session_id.clone(),
                                stop_reason: StopReason::Cancelled,
                                error_message: None,
                                request_id: None,
                            });

                            if let Err(e) =
                                push_session_update_with_project(&project_id, &session_id, notify)
                                    .await
                            {
                                warn!("⚠️ [gRPC] 发送 SessionPromptEnd 通知失败: {}", e);
                            } else {
                                info!(
                                    "📤 [gRPC] 已发送 SessionPromptEnd (Cancelled) 通知: session_id={}",
                                    session_id
                                );
                            }
                        }

                        // 清理 SESSION_CACHE
                        // 🔥 关键修复：先 clone 数据，释放读锁，再调用 .await
                        if let Some(session_data_ref) = SESSION_CACHE.get(&session_id) {
                            let session_data = session_data_ref.clone();
                            drop(session_data_ref); // 显式释放读锁
                            session_data.close_current_connection().await; // ✅ 安全：已无读锁
                        }
                        if SESSION_CACHE.remove(&session_id).is_some() {
                            info!("🗑️ [gRPC] 已清理 SESSION_CACHE: session_id={}", session_id);
                        }

                        // 🎯 使用 remove 原子性地获取 AgentInfo 所有权，避免读锁/写锁竞争
                        let removed_agent_info = AGENT_REGISTRY.remove_by_project(&project_id);

                        if let Some(agent_info) = removed_agent_info {
                            info!(
                                "✅ [gRPC] Agent 已从 Registry 移除: project_id={}",
                                project_id
                            );

                            let response_message =
                                format!("项目 {} 的 Agent 已成功停止", project_id);

                            // 获取 stop_handle 并在后台执行清理
                            if let Some(ref stop_handle) = agent_info.stop_handle {
                                info!("🔪 [gRPC] 主动停止 Agent 子进程: project_id={}", project_id);

                                let stop_handle_clone = Arc::clone(stop_handle);
                                let pid_clone = project_id.clone();

                                // 在后台执行 graceful_stop 和资源清理
                                tokio::spawn(async move {
                                    if let Err(e) = stop_handle_clone.graceful_stop().await {
                                        warn!(
                                            "⚠️ [gRPC] graceful_stop 失败: {}, project_id={}",
                                            e, pid_clone
                                        );
                                    } else {
                                        info!(
                                            "✅ [gRPC] Agent 子进程已停止: project_id={}",
                                            pid_clone
                                        );
                                    }

                                    // 异步 drop AgentInfo
                                    tokio::task::spawn_blocking(move || {
                                        drop(agent_info);
                                        info!(
                                            "🧹 [gRPC] Agent 资源已异步清理完成: project_id={}",
                                            pid_clone
                                        );
                                    });
                                });
                            } else {
                                // 没有 stop_handle，直接在后台 drop
                                let pid_clone = project_id.clone();
                                tokio::spawn(async move {
                                    tokio::task::spawn_blocking(move || {
                                        drop(agent_info);
                                        info!(
                                            "🧹 [gRPC] Agent 资源已异步清理完成: project_id={}",
                                            pid_clone
                                        );
                                    });
                                });
                            }

                            return Ok(Response::new(StopAgentResponse {
                                success: true,
                                result: "stopped".to_string(),
                                message: Some(response_message),
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

                        // ⚠️ 取消失败，Agent 可能还在运行，不发送 SessionPromptEnd

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

                    // 通道关闭说明 Agent 处理线程已崩溃或退出，发送 SessionPromptEnd
                    {
                        use crate::service::push_session_update_with_project;
                        use sacp::schema::StopReason;
                        use shared_types::{SessionNotify, SessionPromptEnd};

                        let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
                            session_id: session_id.clone(),
                            stop_reason: StopReason::Cancelled,
                            error_message: Some(format!("Agent 响应通道关闭: {}", e)),
                            request_id: None,
                        });

                        if let Err(notify_err) =
                            push_session_update_with_project(&project_id, &session_id, notify).await
                        {
                            warn!("⚠️ [gRPC] 发送 SessionPromptEnd 通知失败: {}", notify_err);
                        }

                        // 清理连接
                        // 🔥 关键修复：先 clone 数据，释放读锁，再调用 .await
                        if let Some(session_data_ref) = SESSION_CACHE.get(&session_id) {
                            let session_data = session_data_ref.clone();
                            drop(session_data_ref); // 显式释放读锁
                            session_data.close_current_connection().await; // ✅ 安全：已无读锁
                        }
                    }

                    return Ok(Response::new(StopAgentResponse {
                        success: false,
                        result: "error".to_string(),
                        message: Some(format!("响应通道关闭: {}", e)),
                        project_id,
                    }));
                }
                Err(_) => {
                    warn!("⏰ [gRPC] 等待取消响应超时: project_id={}", project_id);

                    // ⚠️ 超时但 Agent 可能还在运行，不发送 SessionPromptEnd

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

    /// 查询 VNC 服务状态
    ///
    /// 检测容器内 VNC/noVNC 服务是否已启动就绪。
    /// 使用状态标记文件 + 端口检测的双重验证机制。
    #[instrument(skip(self))]
    async fn get_vnc_status(
        &self,
        request: Request<GetVncStatusRequest>,
    ) -> Result<Response<GetVncStatusResponse>, Status> {
        let req = request.into_inner();

        info!(
            "🖥️ [GET_VNC_STATUS] 收到 VNC 状态查询: user_id={:?}, project_id={:?}",
            req.user_id, req.project_id
        );

        // 1. 检查 VNC 就绪标记文件
        let vnc_ready_file = std::path::Path::new("/tmp/vnc_ready");
        let file_exists = vnc_ready_file.exists();

        // 2. 检测端口状态（使用 tokio 异步检测）
        let vnc_port_ready = check_port_available(5900).await;
        let novnc_port_ready = check_port_available(6080).await;

        // 3. 综合判断：标记文件存在 + 端口可达
        let vnc_ready = file_exists && vnc_port_ready;
        let novnc_ready = file_exists && novnc_port_ready;

        // 4. 生成状态消息
        let message = if vnc_ready && novnc_ready {
            "VNC 服务已就绪".to_string()
        } else if file_exists {
            format!(
                "VNC 标记存在，但端口检测异常: vnc={}, novnc={}",
                vnc_port_ready, novnc_port_ready
            )
        } else {
            "VNC 服务未就绪（启动中或启动失败）".to_string()
        };

        let uptime_seconds = self.get_uptime_seconds();

        let response = GetVncStatusResponse {
            vnc_ready,
            novnc_ready,
            message: message.clone(),
            uptime_seconds,
        };

        info!(
            "✅ [GET_VNC_STATUS] 返回状态: vnc_ready={}, novnc_ready={}, message={}, uptime={}s",
            response.vnc_ready, response.novnc_ready, response.message, response.uptime_seconds
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

/// 异步检测端口是否可连接
///
/// 使用 tokio TcpStream 尝试连接指定端口，500ms 超时
async fn check_port_available(port: u16) -> bool {
    use tokio::net::TcpStream;

    match tokio::time::timeout(
        Duration::from_millis(500),
        TcpStream::connect(format!("127.0.0.1:{}", port)),
    )
    .await
    {
        Ok(Ok(_)) => true,
        _ => false,
    }
}
