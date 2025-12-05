//! AgentService gRPC 服务实现
//!
//! 实现 `agent.AgentService` 定义的所有 RPC 方法

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use shared_types::grpc::{
    CancelRequest, CancelResponse, CancelResultType, ChatRequest as GrpcChatRequest,
    ChatResponse as GrpcChatResponse, GetStatusRequest, GetStatusResponse, ProgressEvent,
    ProgressRequest, agent_service_server::AgentService,
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

        let (local_task_request, chat_prompt_rx) = LocalSetAgentRequest::new(prompt_message, None);

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
            // 等待 session 出现并创建连接
            let mut attempts = 0;
            let max_attempts = 30; // 最多等待 30 秒

            loop {
                if let Some(entry) = SESSION_CACHE.get(&session_id_clone) {
                    let session_data = entry.value().clone();
                    drop(entry); // 释放锁

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
                    break;
                }

                attempts += 1;
                if attempts >= max_attempts {
                    warn!("⏰ [gRPC] 等待 session 超时: {}", session_id_clone);
                    let _ = tx
                        .send(Err(Status::deadline_exceeded("Session 等待超时")))
                        .await;
                    break;
                }

                tokio::time::sleep(Duration::from_secs(1)).await;
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
/// 根据 message_type 和 sub_type 映射到具体的 ProgressEvent 类型：
/// - agent_thought_chunk → ThinkingEvent
/// - agent_message_chunk → ChunkEvent
/// - tool_call / tool_call_update → ToolUseEvent
/// - end_turn / prompt_end → CompletionEvent
/// - cancelled / max_tokens → ErrorEvent
/// - 其他 → LogEvent
fn unified_message_to_progress_event(
    message: &shared_types::UnifiedSessionMessage,
) -> ProgressEvent {
    use shared_types::grpc::progress_event::Event;
    use shared_types::grpc::{
        ChunkEvent, CompletionEvent, ErrorEvent, LogEvent, ThinkingEvent, ToolUseEvent,
    };
    use shared_types::SessionMessageType;

    let timestamp = message.timestamp.timestamp_millis();

    let event = match &message.message_type {
        SessionMessageType::AgentSessionUpdate => {
            match message.sub_type.as_str() {
                // 思考过程
                "agent_thought_chunk" => {
                    let thinking_content = message.data.get("thinking")
                        .and_then(|v| v.as_str())
                        .unwrap_or("").to_string();

                    Event::Thinking(ThinkingEvent {
                        content: thinking_content,
                        is_complete: false,
                    })
                }

                // 内容片段
                "agent_message_chunk" => {
                    let content = message.data.get("content")
                        .and_then(|c| c.get("text"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("").to_string();

                    Event::Chunk(ChunkEvent {
                        content,
                        index: 0, // TODO: 从 data 中提取实际索引
                    })
                }

                // 工具调用
                "tool_call" => {
                    let tool_name = message.data.get("tool_call")
                        .and_then(|tc| tc.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("unknown").to_string();

                    let tool_input = message.data.get("tool_call")
                        .and_then(|tc| tc.get("arguments"))
                        .and_then(|args| serde_json::to_string(args).ok())
                        .unwrap_or_default();

                    Event::ToolUse(ToolUseEvent {
                        tool_name,
                        tool_input,
                        tool_output: None,
                        is_error: false,
                    })
                }

                // 工具调用更新
                "tool_call_update" => {
                    let tool_name = message.data.get("tool_call_id")
                        .and_then(|id| id.as_str())
                        .unwrap_or("unknown").to_string();

                    let tool_output = message.data.get("result")
                        .and_then(|r| serde_json::to_string(r).ok())
                        .unwrap_or_default();

                    let is_error = message.data.get("result")
                        .and_then(|r| r.get("status"))
                        .and_then(|s| s.as_str())
                        .map(|s| s != "success")
                        .unwrap_or(false);

                    Event::ToolUse(ToolUseEvent {
                        tool_name,
                        tool_input: String::new(),
                        tool_output: Some(tool_output),
                        is_error,
                    })
                }

                // 其他更新消息作为日志
                _ => Event::Log(LogEvent {
                    level: "info".to_string(),
                    message: format!(
                        "[{}] {}",
                        message.sub_type,
                        serde_json::to_string(&message.data).unwrap_or_default()
                    ),
                }),
            }
        }

        SessionMessageType::SessionPromptEnd => {
            match message.sub_type.as_str() {
                // 正常结束
                "end_turn" | "prompt_end" => {
                    let result = message.data.get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("执行完成").to_string();

                    let total_tokens = message.data.get("total_tokens")
                        .and_then(|t| t.as_i64())
                        .unwrap_or(0) as i32;

                    let duration_ms = message.data.get("duration_ms")
                        .and_then(|d| d.as_i64())
                        .unwrap_or(0);

                    Event::Completion(CompletionEvent {
                        result,
                        total_tokens,
                        duration_ms,
                    })
                }

                // 错误结束
                "cancelled" | "max_tokens" => {
                    let error_code = message.sub_type.clone();
                    let error_message = message.data.get("error_message")
                        .and_then(|e| e.as_str())
                        .or_else(|| message.data.get("message").and_then(|m| m.as_str()))
                        .unwrap_or("执行失败").to_string();

                    Event::Error(ErrorEvent {
                        error_code,
                        error_message,
                        stack_trace: None,
                    })
                }

                _ => Event::Log(LogEvent {
                    level: "info".to_string(),
                    message: format!("会话结束: {}", message.sub_type),
                }),
            }
        }

        SessionMessageType::SessionPromptStart => {
            Event::Log(LogEvent {
                level: "info".to_string(),
                message: "会话开始".to_string(),
            })
        }

        SessionMessageType::Heartbeat => {
            Event::Log(LogEvent {
                level: "debug".to_string(),
                message: "heartbeat".to_string(),
            })
        }
    };

    ProgressEvent {
        event: Some(event),
        timestamp,
    }
}
