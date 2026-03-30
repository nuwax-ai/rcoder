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
use crate::router::AppState;
use crate::service::{AGENT_REGISTRY, SESSION_CACHE};
use crate::{CancelNotificationRequestWrapper, CancelResult};

const ACCEPT_LANGUAGE_METADATA_KEY: &str = "accept-language";

fn locale_from_grpc_request<T>(request: &Request<T>) -> &'static str {
    shared_types::parse_accept_language(
        request
            .metadata()
            .get(ACCEPT_LANGUAGE_METADATA_KEY)
            .and_then(|v| v.to_str().ok()),
    )
}

fn localized(locale: &'static str, zh_cn: &str, zh_tw: &str, en_us: &str) -> String {
    match locale {
        "zh-CN" => zh_cn.to_string(),
        "zh-TW" => zh_tw.to_string(),
        _ => en_us.to_string(),
    }
}

/// Convert gRPC ModelProviderConfig to internal ModelProviderConfig
fn convert_model_provider(grpc_config: GrpcModelProviderConfig) -> ModelProviderConfig {
    ModelProviderConfig {
        id: grpc_config.id, // Keep original ID for session reuse determination
        name: grpc_config.provider,
        base_url: grpc_config.api_base.unwrap_or_default(),
        api_key: grpc_config.api_key.unwrap_or_default(),
        requires_openai_auth: grpc_config.requires_openai_auth.unwrap_or(true),
        default_model: grpc_config.model,
        api_protocol: grpc_config.api_protocol,
    }
}

/// Convert gRPC ChatAgentConfig to internal ChatAgentConfig
fn convert_agent_config(grpc_config: GrpcChatAgentConfig) -> ChatAgentConfig {
    ChatAgentConfig {
        agent_server: grpc_config.agent_server.map(convert_agent_server_config),
        context_servers: grpc_config
            .context_servers
            .into_iter()
            .map(|(k, v)| (k, convert_context_server_config(v)))
            .collect(),
        resource_limits: None, // resource_limits not temporarily passed in gRPC messages
    }
}

/// Convert gRPC ChatAgentServerConfig to internal ChatAgentServerConfig
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

/// Convert gRPC AttachmentSource to internal AttachmentSource
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

/// Convert gRPC Attachment to internal Attachment
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

/// Batch convert attachment list
fn convert_attachments(grpc_attachments: Vec<shared_types::grpc::Attachment>) -> Vec<Attachment> {
    grpc_attachments
        .into_iter()
        .filter_map(convert_attachment)
        .collect()
}

/// gRPC AgentService implementation
pub struct AgentServiceImpl {
    app_state: Arc<AppState>,
}

impl AgentServiceImpl {
    pub fn new(app_state: Arc<AppState>) -> Self {
        Self { app_state }
    }

    /// Get active task count
    ///
    /// Query the number of Agents with Active status in AGENT_REGISTRY
    fn get_active_tasks_count(&self) -> i32 {
        let count = AGENT_REGISTRY
            .iter_agents()
            .filter(|entry| entry.value().status == AgentStatus::Active)
            .count();

        count as i32
    }

    /// Get container uptime (seconds)
    ///
    /// Calculate duration from process start time to now
    fn get_uptime_seconds(&self) -> i64 {
        use std::time::SystemTime;

        // Use process start time as an approximation for container start time
        // Note: This is a simplified implementation, actual container start time should be earlier
        static START_TIME: std::sync::OnceLock<SystemTime> = std::sync::OnceLock::new();

        let start = START_TIME.get_or_init(SystemTime::now);

        match SystemTime::now().duration_since(*start) {
            Ok(duration) => duration.as_secs() as i64,
            Err(_) => {
                warn!("[GET_CONTAINER_STATUS] failed to calculate uptime, returning 0");
                0
            }
        }
    }
}

#[tonic::async_trait]
impl AgentService for AgentServiceImpl {
    /// Chat interface - reuse existing handle_chat core logic
    #[instrument(skip(self, request))]
    async fn chat(
        &self,
        request: Request<GrpcChatRequest>,
    ) -> Result<Response<GrpcChatResponse>, Status> {
        let locale = locale_from_grpc_request(&request);
        let req = request.into_inner();

        // 使用脱敏包装器格式化 model_config
        let model_config_debug = req
            .model_config
            .as_ref()
            .map(shared_types::MaskedModelConfig);

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

        // Validate prompt cannot be empty
        if req.prompt.trim().is_empty() {
            return Err(Status::invalid_argument("prompt field cannot be empty"));
        }

        // 1. 准备参数
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

        let request_id = req
            .request_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string().replace("-", ""));

        // 2. 解析 service_type（默认为 RCoder）
        let service_type = req
            .service_type
            .as_ref()
            .and_then(|st| match st.as_str() {
                "ComputerAgentRunner" => Some(shared_types::ServiceType::ComputerAgentRunner),
                "RCoder" => Some(shared_types::ServiceType::RCoder),
                _ => {
                    warn!("[gRPC] Invalid service_type: {}, using default RCoder", st);
                    None
                }
            })
            .unwrap_or(shared_types::ServiceType::RCoder);

        // 2.1 根据 service_type 计算项目目录（服务端容器内路径）
        let project_dir = match service_type {
            shared_types::ServiceType::ComputerAgentRunner => {
                // ComputerAgentRunner 模式：/home/user/{project_id}
                std::path::PathBuf::from("/home/user").join(&project_id)
            }
            shared_types::ServiceType::RCoder => {
                // RCoder 模式：./project_workspace/{project_id}
                std::path::PathBuf::from("./project_workspace").join(&project_id)
            }
        };

        // 3. 构建 ChatHandlerInput（类型转换）
        use crate::service::{ChatHandlerContext, ChatHandlerInput, handle_chat_core};
        let input = ChatHandlerInput {
            project_id,
            project_dir,
            session_id,
            prompt: req.prompt,
            request_id,
            attachments: convert_attachments(req.attachments),
            data_source_attachments: req.data_source_attachments,
            model_config: req.model_config.map(convert_model_provider),
            service_type,
            agent_config_override: req.agent_config.map(convert_agent_config),
            system_prompt_override: req.system_prompt,
            user_prompt_template_override: req.user_prompt,
            skip_slot_limit: false, // gRPC 请求（Docker 容器部署），继续限制槽位
        };

        // 4. 构建 ChatHandlerContext
        let context = ChatHandlerContext {
            agent_runtime: self.app_state.agent_runtime.clone(),
            shared_api_key_manager: self.app_state.shared_api_key_manager.clone(),
            project_uuid_map: self.app_state.project_uuid_map.clone(),
        };

        // 5. 调用共享 handler（busy check、PendingGuard、session 清理均在 handle_chat_core 内完成）
        let output = shared_types::scope_request_locale(locale, handle_chat_core(input, &context)).await;

        // 6. 转换为 gRPC 响应
        let grpc_response = GrpcChatResponse {
            project_id: output.project_id,
            session_id: output.session_id,
            success: output.success,
            error: output.error,
            error_code: output.error_code,
            request_id: output.request_id,
            need_fallback: output.need_fallback,
            fallback_reason: output.fallback_reason,
        };

        info!("[gRPC] Chat completed: success={}", grpc_response.success);

        Ok(Response::new(grpc_response))
    }

    /// 订阅会话进度流 - Server Streaming RPC
    type SubscribeProgressStream =
        Pin<Box<dyn Stream<Item = Result<ProgressEvent, Status>> + Send>>;

    #[instrument(skip(self, request))]
    async fn subscribe_progress(
        &self,
        request: Request<ProgressRequest>,
    ) -> Result<Response<Self::SubscribeProgressStream>, Status> {
        let locale = locale_from_grpc_request(&request);
        shared_types::scope_request_locale(locale, async move {
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
                    info!("📡 [gRPC] Session connection created successfully: {}", session_id_clone);

                    // 持续接收消息并转换为 ProgressEvent
                    loop {
                        tokio::select! {
                            _ = cancellation_token.cancelled() => {
                                info!("📡 [gRPC] Session connection cancelled, sending SessionPromptEnd: session_id={}", session_id_clone);

                                // ✅ 在断开连接之前，主动发送 SessionPromptEnd 消息
                                use sacp::schema::StopReason;
                                use shared_types::{SessionNotify, SessionPromptEnd};

                                let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
                                    session_id: session_id_clone.clone(),
                                    stop_reason: StopReason::Cancelled,
                                    error_message: Some(localized(
                                        locale,
                                        "用户主动取消任务",
                                        "使用者主動取消任務",
                                        "Task cancelled by user",
                                    )),
                                    request_id: None,
                                });
                                let unified_message = notify.to_unified_message();
                                let end_event = unified_message_to_progress_event(&unified_message);

                                // 发送结束事件（忽略错误，因为客户端可能已经断开）
                                if let Err(e) = tx.send(Ok(end_event)).await {
                                    warn!("📡 [gRPC] Failed to send SessionPromptEnd event: session_id={}, error={}", session_id_clone, e);
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
                                            debug!("📡 [gRPC] Client disconnected");
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
                                        debug!("📡 [gRPC] Session message channel closed, sending SessionPromptEnd event");
                                        // Agent 执行完毕，发送 SessionPromptEnd 事件通知客户端（兜底逻辑）
                                        let end_event = ProgressEvent {
                                            message_type: "SessionPromptEnd".to_string(),
                                            sub_type: "end_turn".to_string(),
                                            payload: format!(
                                                r#"{{"reason":"EndTurn","description":"{}"}}"#,
                                                localized(
                                                    locale,
                                                    "Agent 当前无在执行任务",
                                                    "Agent 目前沒有執行中的任務",
                                                    "Agent has no active task",
                                                )
                                            ),
                                            request_id: None,
                                            timestamp: chrono::Utc::now().timestamp_millis(),
                                        };
                                        if let Err(e) = tx.send(Ok(end_event)).await {
                                            warn!("📡 [gRPC] Failed to send SessionPromptEnd event: session_id={}, error={}", session_id_clone, e);
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
                                    debug!("📡 [gRPC] Failed to send heartbeat; client disconnected");
                                    break;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("[gRPC] Failed to create session connection: {}", e);
                    if let Err(send_err) = tx
                        .send(Err(Status::internal(format!(
                            "{}: {}",
                            localized(
                                locale,
                                "创建连接失败",
                                "建立連線失敗",
                                "Failed to create connection",
                            ),
                            e
                        ))))
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
        })
        .await
    }

    /// Cancel session task
    #[instrument(skip(self, request))]
    async fn cancel_session(
        &self,
        request: Request<CancelRequest>,
    ) -> Result<Response<CancelResponse>, Status> {
        let locale = locale_from_grpc_request(&request);
        shared_types::scope_request_locale(locale, async move {
            let req = request.into_inner();
            info!(
                "🛑 [gRPC] CancelSession: session_id={}, project_id={}, reason={}",
                req.session_id, req.project_id, req.reason
            );

        // 🔧 Determine actual session_id
        // When session_id is empty, look up by project_id
        let actual_session_id = if req.session_id.is_empty() {
            info!(
                "📝 [gRPC] session_id is empty, looking up by project_id={}",
                req.project_id
            );

            match AGENT_REGISTRY.get_agent_info(&req.project_id) {
                Some(info) => {
                    let sid = info.session_id.to_string();
                    info!(
                        "✅ [gRPC] got session_id={} from project_id={}",
                        req.project_id, sid
                    );
                    sid
                }
                None => {
                    info!(
                        "ℹ️ [gRPC] project_id={} has no active session, cancel target achieved",
                        req.project_id
                    );
                    return Ok(Response::new(CancelResponse {
                        success: true,
                        result: CancelResultType::CancelResultSuccess as i32,
                        message: Some(localized(
                            locale,
                            "项目当前没有活跃会话",
                            "專案目前沒有活躍工作階段",
                            "Project has no active session",
                        )),
                    }));
                }
            }
        } else {
            req.session_id.clone()
        };

        // 1. Get project_id via unified Registry O(1) reverse query
        let project_id = match AGENT_REGISTRY.get_project_by_session(&actual_session_id) {
            Some(pid) => {
                debug!(
                    "✅ [gRPC] found project_id={} for session_id={}",
                    actual_session_id, pid
                );
                pid
            }
            None => {
                warn!(
                    "⚠️ [gRPC] found no project for session_id={}",
                    actual_session_id
                );
                // Session doesn't exist or already completed, return success (idempotent design)
                return Ok(Response::new(CancelResponse {
                    success: true,
                    result: CancelResultType::CancelResultSuccess as i32,
                    message: Some(localized(
                        locale,
                        "会话不存在或已完成",
                        "工作階段不存在或已完成",
                        "Session does not exist or already completed",
                    )),
                }));
            }
        };

        // 2. Get agent_info and extract needed data
        // 🔥 Critical fix: use code block to limit read lock lifetime, avoid deadlock when holding read lock across .await
        let (status, cancel_tx) = {
            let agent_info = match AGENT_REGISTRY.get_agent_info(&project_id) {
                Some(info) => info,
                None => {
                    info!(
                        "ℹ️ [gRPC] project_id={} has no active session, cancel target achieved (idempotent)",
                        project_id
                    );
                    return Ok(Response::new(CancelResponse {
                        success: true,
                        result: CancelResultType::CancelResultSuccess as i32,
                        message: Some(localized(
                            locale,
                            "项目当前没有活跃会话",
                            "專案目前沒有活躍工作階段",
                            "Project has no active session",
                        )),
                    }));
                }
            };

            // ✅ Proactively clone data, then explicitly drop read lock
            // status is Copy type, directly copied
            // cancel_tx is UnboundedSender, very cheap to clone (internal Arc ref count +1 only)
            let status = agent_info.status;
            let cancel_tx = agent_info.cancel_tx.clone();

            // 🔥 Explicitly release read lock, ensure released before code block ends
            drop(agent_info);

            (status, cancel_tx)
        };

        // 🆕 ===== Idempotency check and channel validity verification =====
        match status {
            AgentStatus::Idle => {
                // Already Idle status, no need to cancel (idempotent return)
                info!(
                    "✅ [gRPC] Agent already in Idle status, cancel request idempotent success: project_id={}, session_id={}",
                    project_id, actual_session_id
                );
                return Ok(Response::new(CancelResponse {
                    success: true,
                    result: CancelResultType::CancelResultSuccess as i32,
                    message: Some(localized(
                        locale,
                        "Agent 已处于空闲状态",
                        "Agent 已處於閒置狀態",
                        "Agent already in idle status",
                    )),
                }));
            }
            AgentStatus::Terminating => {
                // Already stopping, no need to cancel again (idempotent return)
                info!(
                    "✅ [gRPC] Agent already stopping, cancel request idempotent success: project_id={}, session_id={}",
                    project_id, actual_session_id
                );
                return Ok(Response::new(CancelResponse {
                    success: true,
                    result: CancelResultType::CancelResultSuccess as i32,
                    message: Some(localized(
                        locale,
                        "Agent 已在停止中",
                        "Agent 已在停止中",
                        "Agent is already stopping",
                    )),
                }));
            }
            AgentStatus::Active | AgentStatus::Pending => {
                // Normal flow: continue with cancellation
                debug!(
                    "🔄 [gRPC] Agent status is {:?}, executing cancel: project_id={}, session_id={}",
                    status, project_id, actual_session_id
                );
            }
        }

        // 🆕 Verify cancel_tx channel is still valid
        // Avoid: LocalSet exited causing cancel_tx to be invalid, but only discovered at send time
        if cancel_tx.is_closed() {
            error!(
                "❌ [gRPC] cancel_tx channel closed, LocalSet may have unexpectedly exited: project_id={}, session_id={}",
                project_id, actual_session_id
            );
            return Ok(Response::new(CancelResponse {
                success: false,
                result: CancelResultType::CancelResultFailed as i32,
                message: Some(localized(
                    locale,
                    "取消通道已关闭，Agent 可能已停止",
                    "取消通道已關閉，Agent 可能已停止",
                    "Cancel channel closed, Agent may have stopped",
                )),
            }));
        }

        // 3. Create SessionId and CancelNotification
        let session_id_obj = SessionId::new(Arc::from(actual_session_id.as_str()));
        let cancel_notification = CancelNotification::new(session_id_obj);

        // 4. Create oneshot channel to wait for cancel result
        let (result_tx, result_rx) = oneshot::channel::<CancelResult>();
        let cancel_request = CancelNotificationRequestWrapper {
            cancel_notification,
            result_tx,
        };

        // 5. Send cancel notification (using already extracted cancel_tx, with backpressure)
        if let Err(e) = cancel_tx.send(cancel_request).await {
            error!("[gRPC] failed to send cancel notification: {}", e);
            return Ok(Response::new(CancelResponse {
                success: false,
                result: CancelResultType::CancelResultFailed as i32,
                message: Some(format!(
                    "{}: {}",
                    localized(
                        locale,
                        "发送取消通知失败",
                        "發送取消通知失敗",
                        "Failed to send cancel notification",
                    ),
                    e
                )),
            }));
        }

        info!(
            "📡 [gRPC] waiting for Agent cancel response: session_id={}",
            actual_session_id
        );

        // 6. Wait for cancel response (with timeout)
        let cancel_timeout_secs = self
            .app_state
            .config
            .grpc_timeouts
            .as_ref()
            .map(|t| t.cancel_session_timeout_secs)
            .unwrap_or(30); // Default 30 seconds
        match tokio::time::timeout(Duration::from_secs(cancel_timeout_secs), result_rx).await {
            Ok(Ok(cancel_result)) => {
                let is_success = cancel_result.is_success();
                debug!(
                    "✅ [gRPC] received Agent cancel response: session_id={}, success={}",
                    actual_session_id, is_success
                );

                if is_success {
                    // 取消成功，继续后续清理逻辑
                } else {
                    // 取消失败，Agent 可能还在运行，不发送 SessionPromptEnd
                    return Ok(Response::new(CancelResponse {
                        success: false,
                        result: CancelResultType::CancelResultFailed as i32,
                        message: Some(localized(
                            locale,
                            "Agent 取消执行失败",
                            "Agent 取消執行失敗",
                            "Agent cancel execution failed",
                        )),
                    }));
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
                    error_message: Some(format!(
                        "{}: {}",
                        localized(
                            locale,
                            "Agent 响应通道关闭",
                            "Agent 回應通道關閉",
                            "Agent response channel closed",
                        ),
                        e
                    )),
                    request_id: None,
                });

                if let Err(notify_err) =
                    push_session_update_with_project(&project_id, &actual_session_id, notify).await
                {
                    warn!("[gRPC] Failed to send SessionPromptEnd notification: {}", notify_err);
                }

                // 清理连接
                if let Some(session_data_ref) = SESSION_CACHE.get(&actual_session_id) {
                    let session_data = session_data_ref.clone();
                    drop(session_data_ref); // 显式释放读锁
                    session_data.close_current_connection().await;
                }

                return Ok(Response::new(CancelResponse {
                    success: false,
                    result: CancelResultType::CancelResultFailed as i32,
                    message: Some(format!(
                        "{}: {}",
                        localized(locale, "响应通道关闭", "回應通道關閉", "Response channel closed"),
                        e
                    )),
                }));
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
                    error_message: Some(localized(
                        locale,
                        "取消请求超时，主动清理资源",
                        "取消請求逾時，主動清理資源",
                        "Cancel request timed out; resources were cleaned up",
                    )),
                    request_id: None,
                });

                if let Err(e) =
                    push_session_update_with_project(&project_id, &actual_session_id, notify).await
                {
                    warn!("[gRPC] Failed to send SessionPromptEnd notification: {}", e);
                } else {
                    info!(
                        "📤 [gRPC] 已发送 SessionPromptEnd (Timeout) 通知: session_id={}",
                        actual_session_id
                    );
                }

                // 2. 清理 SESSION_CACHE
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
                use chrono::Utc;
                use shared_types::AgentStatus;

                let updated = AGENT_REGISTRY.try_update_agent_info(&project_id, |info| {
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
                }

                return Ok(Response::new(CancelResponse {
                    success: false,
                    result: CancelResultType::CancelResultTimeout as i32,
                    message: Some(localized(
                        locale,
                        "取消请求超时（30秒）",
                        "取消請求逾時（30 秒）",
                        "Cancel request timed out (30 seconds)",
                    )),
                }));
            }
        }; // match tokio::time::timeout

        // 取消成功后的统一清理逻辑
        // 主动发送 SessionPromptEnd 通知 SSE 客户端任务已取消
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
                push_session_update_with_project(&project_id, &actual_session_id, notify).await
            {
                warn!("[gRPC] Failed to send SessionPromptEnd notification: {}", e);
            } else {
                info!(
                    "📤 [gRPC] 已发送 SessionPromptEnd (Cancelled) 通知: session_id={}",
                    actual_session_id
                );
            }

            // 清理连接
            if let Some(session_data_ref) = SESSION_CACHE.get(&actual_session_id) {
                let session_data = session_data_ref.clone();
                drop(session_data_ref);
                session_data.close_current_connection().await;
            }
            if SESSION_CACHE.remove(&actual_session_id).is_some() {
                info!(
                    "🗑️ [gRPC] 已清理 SESSION_CACHE: session_id={}",
                    actual_session_id
                );
            }

            // 更新状态为 Idle（如果当前是 Active/Pending）
            use chrono::Utc;
            use shared_types::AgentStatus;

            let updated = AGENT_REGISTRY.try_update_agent_info(&project_id, |info| {
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
                    "✅ [gRPC] Agent 状态已原子性更新为 Idle: project_id={}",
                    project_id
                );
            }

        Ok(Response::new(CancelResponse {
            success: true,
            result: CancelResultType::CancelResultSuccess as i32,
            message: Some(localized(
                locale,
                "取消成功",
                "取消成功",
                "Cancelled successfully",
            )),
        }))
        })
        .await
    }

    /// 获取 Agent 状态
    ///
    /// 支持通过 `project_id` 或 `session_id` 查询 Agent 状态
    #[instrument(skip(self, request))]
    async fn get_status(
        &self,
        request: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        let locale = locale_from_grpc_request(&request);
        shared_types::scope_request_locale(locale, async move {
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
            info!("📊 [gRPC] GetStatus: all parameters are empty, returning not_found");
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
        })
        .await
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

        let locale = locale_from_grpc_request(&request);
        shared_types::scope_request_locale(locale, async move {
            let req = request.into_inner();
            let project_id = req.project_id.clone();
            let force = req.force;
            let reason = req
                .reason
                .clone()
                .unwrap_or_else(|| {
                    localized(
                        locale,
                        "用户请求停止",
                        "使用者請求停止",
                        "Stop requested by user",
                    )
                });

        info!(
            "🛑 [gRPC] StopAgent: project_id={}, force={}, reason={}",
            project_id, force, reason
        );

        // 检查 Agent 是否存在，并提取需要的数据后立即释放读锁
        // ⚠️ 重要：必须在任何 .await 之前 drop 掉 Ref，否则会导致死锁
        let (agent_status, session_id, cancel_tx) = match AGENT_REGISTRY.get_agent_info(&project_id)
        {
            Some(info) => {
                let status = info.status;
                let session_id = info.session_id.to_string();
                let cancel_tx = info.cancel_tx.clone();
                // info（Ref）在这里被 drop，释放读锁
                (status, session_id, cancel_tx)
            }
            None => {
                info!("📭 [gRPC] Agent not found: project_id={}", project_id);
                return Ok(Response::new(StopAgentResponse {
                    success: true,
                    result: "not_found".to_string(),
                    message: Some(format!(
                        "{} {}",
                        localized(
                            locale,
                            "项目的 Agent 不存在或已停止:",
                            "專案的 Agent 不存在或已停止:",
                            "Agent not found or already stopped for project:",
                        ),
                        project_id
                    )),
                    project_id,
                }));
            }
        };

        // 如果 Agent 已经在 Terminating 状态，返回 already_stopped
        if agent_status == AgentStatus::Terminating {
            info!("ℹ️ [gRPC] Agent is already stopping: project_id={}", project_id);

            // 🆕 即使已在停止中，也要发送 SessionPromptEnd 确保前端收到结束消息
            if !session_id.is_empty() {
                use crate::service::push_session_update_with_project;
                use sacp::schema::StopReason;
                use shared_types::{SessionNotify, SessionPromptEnd};

                let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
                    session_id: session_id.clone(),
                    stop_reason: StopReason::Cancelled,
                    error_message: Some(localized(
                        locale,
                        "Agent 已在停止中",
                        "Agent 已在停止中",
                        "Agent is already stopping",
                    )),
                    request_id: None,
                });

                if let Err(e) =
                    push_session_update_with_project(&project_id, &session_id, notify).await
                {
                    warn!("[gRPC] Failed to send SessionPromptEnd notification: {}", e);
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
                message: Some(format!(
                    "{} {}",
                    localized(
                        locale,
                        "项目的 Agent 已在停止中:",
                        "專案的 Agent 已在停止中:",
                        "Agent is already stopping for project:",
                    ),
                    project_id
                )),
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
                    warn!("[gRPC] Failed to send SessionPromptEnd notification: {}", e);
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
                    info!("🗑️ [gRPC] SESSION_CACHE cleared: session_id={}", session_id);
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
                    info!("🔪 [gRPC] Force-stopping Agent child process: project_id={}", project_id);

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
                            info!("[gRPC] Agent child process stopped: project_id={}", pid_clone);
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
            let response_message = format!(
                "{} {}",
                localized(
                    locale,
                    "项目的 Agent 正在停止（后台清理中）:",
                    "專案的 Agent 正在停止（後台清理中）:",
                    "Agent is stopping (background cleanup in progress) for project:",
                ),
                project_id
            );

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

            // 发送取消通知（使用之前提取的 cancel_tx，支持背压）
            if let Err(e) = cancel_tx.send(cancel_request).await {
                error!(
                    "❌ [gRPC] 发送取消通知失败: project_id={}, error={}",
                    project_id, e
                );
                return Ok(Response::new(StopAgentResponse {
                    success: false,
                    result: "error".to_string(),
                    message: Some(format!(
                        "{}: {}",
                        localized(
                            locale,
                            "发送取消通知失败",
                            "發送取消通知失敗",
                            "Failed to send cancel notification",
                        ),
                        e
                    )),
                    project_id,
                }));
            }

            // 等待取消响应（带超时）
            let cancel_timeout_secs = self
                .app_state
                .config
                .grpc_timeouts
                .as_ref()
                .map(|t| t.cancel_session_timeout_secs)
                .unwrap_or(30); // 默认 30 秒
            match tokio::time::timeout(Duration::from_secs(cancel_timeout_secs), result_rx).await {
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
                                warn!("[gRPC] Failed to send SessionPromptEnd notification: {}", e);
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
                            info!("🗑️ [gRPC] SESSION_CACHE cleared: session_id={}", session_id);
                        }

                        // 🎯 使用 remove 原子性地获取 AgentInfo 所有权，避免读锁/写锁竞争
                        let removed_agent_info = AGENT_REGISTRY.remove_by_project(&project_id);

                        if let Some(agent_info) = removed_agent_info {
                            info!(
                                "✅ [gRPC] Agent 已从 Registry 移除: project_id={}",
                                project_id
                            );

                            let response_message = format!(
                                "{} {}",
                                localized(
                                    locale,
                                    "项目的 Agent 已成功停止:",
                                    "專案的 Agent 已成功停止:",
                                    "Agent stopped successfully for project:",
                                ),
                                project_id
                            );

                            // 获取 stop_handle 并在后台执行清理
                            if let Some(ref stop_handle) = agent_info.stop_handle {
                                info!("🔪 [gRPC] Force-stopping Agent child process: project_id={}", project_id);

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
                                message: Some(localized(
                                    locale,
                                    "移除 Agent 失败",
                                    "移除 Agent 失敗",
                                    "Failed to remove Agent",
                                )),
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
                            message: Some(localized(
                                locale,
                                "取消会话失败",
                                "取消工作階段失敗",
                                "Failed to cancel session",
                            )),
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
                            error_message: Some(format!(
                                "{}: {}",
                                localized(
                                    locale,
                                    "Agent 响应通道关闭",
                                    "Agent 回應通道關閉",
                                    "Agent response channel closed",
                                ),
                                e
                            )),
                            request_id: None,
                        });

                        if let Err(notify_err) =
                            push_session_update_with_project(&project_id, &session_id, notify).await
                        {
                            warn!("[gRPC] Failed to send SessionPromptEnd notification: {}", notify_err);
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
                        message: Some(format!(
                            "{}: {}",
                            localized(
                                locale,
                                "响应通道关闭",
                                "回應通道關閉",
                                "Response channel closed",
                            ),
                            e
                        )),
                        project_id,
                    }));
                }
                Err(_) => {
                    warn!("⏰ [gRPC] Timed out waiting for cancel response: project_id={}", project_id);

                    // ⚠️ 超时但 Agent 可能还在运行，不发送 SessionPromptEnd

                    return Ok(Response::new(StopAgentResponse {
                        success: false,
                        result: "error".to_string(),
                        message: Some(localized(
                            locale,
                            "取消请求超时（30秒）",
                            "取消請求逾時（30 秒）",
                            "Cancel request timed out (30 seconds)",
                        )),
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
                message: Some(localized(
                    locale,
                    "意外的代码分支",
                    "意外的程式分支",
                    "Unexpected code path",
                )),
                project_id,
            }))
        })
        .await
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
        let locale = locale_from_grpc_request(&request);
        shared_types::scope_request_locale(locale, async move {
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
        })
        .await
    }

    /// 查询 VNC 服务状态
    ///
    /// 检测容器内 VNC/noVNC 服务是否已启动就绪。
    /// 使用状态标记文件 + noVNC 端口检测的双重验证机制。
    ///
    /// 注意：不再 TCP 探测 Xvnc 5900 端口，因为 TCP 连接会触发 TigerVNC 的
    /// 客户端管理逻辑（DisconnectClients），可能导致正在连接的前端用户被踢掉。
    /// /tmp/vnc_ready 标记文件本身已经包含了 5900 端口可达的验证结果。
    #[instrument(skip(self))]
    async fn get_vnc_status(
        &self,
        request: Request<GetVncStatusRequest>,
    ) -> Result<Response<GetVncStatusResponse>, Status> {
        let locale = locale_from_grpc_request(&request);
        shared_types::scope_request_locale(locale, async move {
            let req = request.into_inner();

        info!(
            "🖥️ [GET_VNC_STATUS] 收到 VNC 状态查询: user_id={:?}, project_id={:?}",
            req.user_id, req.project_id
        );

        // 1. 检查 VNC 就绪标记文件
        // /tmp/vnc_ready 由 start-up.sh 的 wait_and_write_vnc_ready_marker() 写入，
        // 写入前已验证：Xvnc 5900 可达 + noVNC 6080 可达 + WebSocket 101 + 壁纸就绪 + xfdesktop 运行
        let vnc_ready_file = std::path::Path::new("/tmp/vnc_ready");
        let file_exists = vnc_ready_file.exists();

        // 2. 仅检测 noVNC 6080 端口（面向前端的 WebSocket 代理端口）
        // 不再探测 Xvnc 5900 端口，避免 TCP 连接干扰 VNC 客户端会话
        let port_check_timeout = self
            .app_state
            .config
            .grpc_timeouts
            .as_ref()
            .map(|t| t.port_check_timeout_millis)
            .unwrap_or(500); // 默认 500 毫秒
        let novnc_port_ready = check_port_available(6080, port_check_timeout).await;

        // 3. 综合判断
        // vnc_ready: 标记文件存在即可（标记写入时已验证 5900 端口）
        // novnc_ready: 标记文件存在 + 6080 端口可达
        let vnc_ready = file_exists;
        let novnc_ready = file_exists && novnc_port_ready;

        // 4. 生成状态消息
        let message = if vnc_ready && novnc_ready {
            localized(locale, "VNC 服务已就绪", "VNC 服務已就緒", "VNC service is ready")
        } else if file_exists && !novnc_port_ready {
            localized(
                locale,
                "VNC 标记存在，但 noVNC 端口 6080 不可达",
                "VNC 標記存在，但 noVNC 埠 6080 無法連線",
                "VNC marker exists, but noVNC port 6080 is unreachable",
            )
        } else {
            localized(
                locale,
                "VNC 服务未就绪（启动中或启动失败）",
                "VNC 服務尚未就緒（啟動中或啟動失敗）",
                "VNC service is not ready (starting or failed)",
            )
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
        })
        .await
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
/// 使用 tokio TcpStream 尝试连接指定端口
async fn check_port_available(port: u16, timeout_millis: u64) -> bool {
    use tokio::net::TcpStream;

    match tokio::time::timeout(
        Duration::from_millis(timeout_millis),
        TcpStream::connect(format!("127.0.0.1:{}", port)),
    )
    .await
    {
        Ok(Ok(_)) => true,
        _ => false,
    }
}
