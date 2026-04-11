//! gRPC SSE 流处理器
//!
//! 通过 gRPC SubscribeProgress 接收 agent_runner 的进度事件，
//! 并转换为 SSE 事件返回给客户端

use chrono::{DateTime, Utc};
use shared_types::grpc::{GetStatusRequest, ProgressRequest};
use shared_types::{SessionMessageType, UnifiedSessionMessage};
use tonic::Code;
use tracing::{debug, error, info, warn};

/// 创建基于 gRPC 的 SSE 代理流
///
/// 通过 gRPC `SubscribeProgress` 方法订阅 agent_runner 的进度事件，
/// 并将事件转换为 SSE 格式返回
///
/// 🚀 优化：使用连接池 + 智能重试机制
/// 🆕 新增：在建立流之前检查 Agent 状态，如果 Agent 闲置则直接发送 SessionPromptEnd 并关闭
pub async fn create_grpc_sse_stream(
    grpc_addr: String,
    session_id: String,
    project_id: String,
    pool: std::sync::Arc<crate::grpc::GrpcChannelPool>,
    locale: &'static str,
) -> impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
{
    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let session_id_clone = session_id.clone();

    // 在后台任务中处理 gRPC 流
    tokio::spawn(async move {
        info!(
            "🔗 [gRPC_SSE] Starting connection to agent_runner gRPC: addr={}, session_id={}, project_id={}",
            grpc_addr, session_id_clone, project_id
        );

        let max_retries = 2;
        let mut last_error_msg = String::new();

        for attempt in 1..=max_retries {
            // 1. 从连接池获取客户端
            let mut client = match pool.get_client(&grpc_addr).await {
                Ok(client) => client,
                Err(e) => {
                    warn!(
                        "⚠️ [gRPC_SSE] Failed to get client (attempt {}/{}): {}, cleaning connection pool and retrying...",
                        attempt, max_retries, e
                    );
                    pool.remove(&grpc_addr);
                    last_error_msg = format!("failed to get client: {}", e);
                    continue;
                }
            };

            // 🆕 2. 先检查 Agent 状态（使用 session_id 查询）
            let status_request = crate::grpc::new_request_with_locale(
                GetStatusRequest {
                    project_id: String::new(),            // 不使用 project_id
                    session_id: session_id_clone.clone(), // 使用 session_id 查询
                },
                locale,
            );

            match client.get_status(status_request).await {
                Ok(response) => {
                    let status = response.into_inner().status;
                    if status == "idle" {
                        // Agent 闲置，发送 SessionPromptEnd 并关闭连接
                        info!(
                            "💤 [gRPC_SSE] Agent is idle, sending SessionPromptEnd and closing: session_id={}",
                            session_id_clone
                        );
                        let end_event = create_session_prompt_end_event(&session_id_clone);
                        if let Err(e) = tx.send(Ok(end_event)).await {
                            warn!(
                                "⚠️ [gRPC_SSE] Failed to send SessionPromptEnd event: session_id={}, error={}",
                                session_id_clone, e
                            );
                        }
                        return; // 直接结束，不建立流
                    }
                    info!(
                        "🔄 [gRPC_SSE] Agent status is {}, continuing to establish stream: session_id={}",
                        status, session_id_clone
                    );
                }
                Err(e) => {
                    // 状态检查失败，记录警告但继续尝试建立流
                    warn!(
                        "⚠️ [gRPC_SSE] Agent status check failed: {}, continuing to try establishing stream: session_id={}",
                        e, session_id_clone
                    );
                }
            }

            // 3. 发送 SubscribeProgress 请求
            let request = crate::grpc::new_request_with_locale(
                ProgressRequest {
                    session_id: session_id_clone.clone(),
                },
                locale,
            );

            match client.subscribe_progress(request).await {
                Ok(response) => {
                    info!(
                        "✅ [gRPC_SSE] Successfully established SubscribeProgress stream: session_id={}",
                        session_id_clone
                    );

                    let mut stream = response.into_inner();

                    // 持续接收 gRPC 流中的事件
                    loop {
                        match stream.message().await {
                            Ok(Some(progress_event)) => {
                                debug!(
                                    "📨 [gRPC_SSE] Received progress event: session_id={}, message_type={}, sub_type={}",
                                    session_id_clone,
                                    progress_event.message_type,
                                    progress_event.sub_type
                                );

                                // 将 ProgressEvent 转换为 SSE Event（传入 session_id 以重建完整消息结构）
                                let sse_event =
                                    progress_event_to_sse(&progress_event, &session_id_clone);

                                if tx.send(Ok(sse_event)).await.is_err() {
                                    warn!(
                                        "⚠️ [gRPC_SSE] Client disconnected: session_id={}",
                                        session_id_clone
                                    );
                                    // 客户端断开，直接退出任务
                                    return;
                                }
                            }
                            Ok(None) => {
                                // 流正常结束（agent_runner 主动关闭）
                                info!(
                                    "✅ [gRPC_SSE] gRPC stream ended normally: session_id={}",
                                    session_id_clone
                                );
                                return;
                            }
                            Err(e) => {
                                // 流异常结束（连接中断、超时等）
                                error!(
                                    "❌ [gRPC_SSE] gRPC stream error: session_id={}, code={}, message={}",
                                    session_id_clone,
                                    e.code(),
                                    e.message()
                                );

                                // 发送标准格式的错误消息
                                let error_event = create_grpc_stream_error_event(
                                    &session_id_clone,
                                    e.code(),
                                    e.message(),
                                );
                                if let Err(e) = tx.send(Ok(error_event)).await {
                                    warn!(
                                        "⚠️ [gRPC_SSE] Failed to send error event: session_id={}, error={}",
                                        session_id_clone, e
                                    );
                                }
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "⚠️ [gRPC_SSE] SubscribeProgress call failed (attempt {}/{}): {}",
                        attempt, max_retries, e
                    );

                    // 如果不是最后一次尝试，清理连接池并重试
                    if attempt < max_retries {
                        info!(
                            "🔌 [gRPC_SSE] Possibly connection broken, removing {} from connection pool and retrying...",
                            grpc_addr
                        );
                        pool.remove(&grpc_addr);
                        last_error_msg = format!("stream subscription failed: {}", e);
                        continue;
                    }

                    last_error_msg = format!("stream subscription ultimately failed: {}", e);
                }
            }
        }

        // 如果循环结束还没有 return，说明所有重试都失败了
        error!(
            "❌ [gRPC_SSE] Retried {} times ultimately failed: session_id={}, error={}",
            max_retries, session_id_clone, last_error_msg
        );

        let error_event = create_connection_error_event(&session_id_clone, &last_error_msg);
        if let Err(e) = tx.send(Ok(error_event)).await {
            warn!(
                "⚠️ [gRPC_SSE] Failed to send error event: session_id={}, error={}",
                session_id_clone, e
            );
        }
    });

    tokio_stream::wrappers::ReceiverStream::new(rx)
}

/// 创建 Agent 闲置时的 SessionPromptEnd SSE 事件
///
/// 当 Agent 处于闲置状态时，发送此事件通知前端没有正在执行的任务
fn create_session_prompt_end_event(session_id: &str) -> axum::response::sse::Event {
    let unified_message = UnifiedSessionMessage {
        session_id: session_id.to_string(),
        message_type: SessionMessageType::SessionPromptEnd,
        sub_type: "end_turn".to_string(),
        data: serde_json::json!({
            "reason": "EndTurn",
            "description": "Agent has no task in execution"
        }),
        timestamp: Utc::now(),
    };

    let json_data = match serde_json::to_string(&unified_message) {
        Ok(json) => json,
        Err(e) => {
            warn!(
                "⚠️ [gRPC_SSE] Failed to serialize SessionPromptEnd message: {}, error={}",
                session_id, e
            );
            // 返回包含 session_id 的最小可用结构
            format!(
                r#"{{"session_id":"{}","message_type":"SessionPromptEnd","sub_type":"end_turn","data":null}}"#,
                session_id
            )
        }
    };

    axum::response::sse::Event::default()
        .event("prompt_end")
        .data(json_data)
}

/// 将 gRPC ProgressEvent 转换为 SSE Event
///
/// 使用 UnifiedSessionMessage 结构体重建完整消息，包含 sessionId、messageType、subType、data、timestamp
/// 使用 sub_type 作为 SSE 事件名，前端通过 eventSource.addEventListener(sub_type, ...) 监听
fn progress_event_to_sse(
    event: &shared_types::grpc::ProgressEvent,
    session_id: &str,
) -> axum::response::sse::Event {
    // 解析 payload 为 data 字段
    let data: serde_json::Value =
        serde_json::from_str(&event.payload).unwrap_or(serde_json::Value::Null);

    // 将 gRPC 时间戳（毫秒）转换为 DateTime<Utc>
    let timestamp = match DateTime::<Utc>::from_timestamp_millis(event.timestamp) {
        Some(ts) => ts,
        None => {
            warn!(
                "⚠️ [gRPC_SSE] Invalid timestamp: session_id={}, timestamp={}, using current time",
                session_id, event.timestamp
            );
            Utc::now()
        }
    };

    // 将 message_type 字符串转换为 SessionMessageType 枚举
    let message_type = parse_message_type(&event.message_type);

    // 使用 UnifiedSessionMessage 结构体构建完整消息
    let unified_message = UnifiedSessionMessage {
        session_id: session_id.to_string(),
        message_type,
        sub_type: event.sub_type.clone(),
        data,
        timestamp,
    };

    // 序列化为 JSON
    let json_data = match serde_json::to_string(&unified_message) {
        Ok(json) => json,
        Err(e) => {
            warn!(
                "⚠️ [gRPC_SSE] Failed to serialize ProgressEvent message: session_id={}, message_type={}, error={}",
                session_id, event.message_type, e
            );
            // 返回包含 session_id 的最小可用结构
            format!(
                r#"{{"session_id":"{}","message_type":"Unknown","sub_type":"{}","data":null}}"#,
                session_id, event.sub_type
            )
        }
    };

    // 使用 sub_type 作为 SSE 事件名
    // 前端通过 eventSource.addEventListener('agent_message_chunk', ...) 等方式监听
    axum::response::sse::Event::default()
        .event(&event.sub_type)
        .data(json_data)
}

/// 将 message_type 字符串解析为 SessionMessageType 枚举
///
/// 支持的格式：
/// - "SessionPromptStart" -> SessionMessageType::SessionPromptStart
/// - "SessionPromptEnd" -> SessionMessageType::SessionPromptEnd
/// - "AgentSessionUpdate" -> SessionMessageType::AgentSessionUpdate
/// - "Heartbeat" -> SessionMessageType::Heartbeat
fn parse_message_type(message_type: &str) -> SessionMessageType {
    match message_type {
        "SessionPromptStart" => SessionMessageType::SessionPromptStart,
        "SessionPromptEnd" => SessionMessageType::SessionPromptEnd,
        "AgentSessionUpdate" => SessionMessageType::AgentSessionUpdate,
        "Heartbeat" => SessionMessageType::Heartbeat,
        // 默认作为 AgentSessionUpdate 处理
        _ => {
            debug!(
                "⚠️ [gRPC_SSE] Unknown message_type: {}, using AgentSessionUpdate as default",
                message_type
            );
            SessionMessageType::AgentSessionUpdate
        }
    }
}

/// 获取容器的 gRPC 地址
///
/// 返回格式: `{container_ip}:{grpc_port}`
/// 默认 gRPC 端口为 50051
pub async fn get_container_grpc_addr(project_id: &str, grpc_port: u16) -> anyhow::Result<String> {
    info!(
        "🔍 [CONTAINER] Getting container gRPC address: project_id={}",
        project_id
    );

    // 获取全局 DockerManager 实例
    let docker_manager = docker_manager::global::get_global_docker_manager()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get global DockerManager: {}", e))?;

    // 使用高级 API 获取容器信息（包含 IP）
    let agent_info = docker_manager
        .get_agent_info(project_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Container info not found: project_id={}", project_id))?;

    let grpc_addr = format!("{}:{}", agent_info.container_ip, grpc_port);

    info!("[CONTAINER] get container gRPC addr: {}", grpc_addr);
    Ok(grpc_addr)
}

/// 创建 gRPC 流异常错误事件
///
/// 当 gRPC 流在传输过程中异常结束时发送此事件
fn create_grpc_stream_error_event(
    session_id: &str,
    code: Code,
    message: &str,
) -> axum::response::sse::Event {
    // 使用项目标准的错误码映射
    let error_code = map_tonic_code_to_error_code(code);

    let unified_message = UnifiedSessionMessage {
        session_id: session_id.to_string(),
        message_type: SessionMessageType::SessionPromptEnd,
        sub_type: "error".to_string(),
        data: serde_json::json!({
            "code": error_code,
            "message": "Agent computer execution error, please retry (tasks consuming too much memory may cause the agent computer process to terminate).",
        }),
        timestamp: Utc::now(),
    };

    let json_data = match serde_json::to_string(&unified_message) {
        Ok(json) => json,
        Err(e) => {
            warn!(
                "⚠️ [gRPC_SSE] Failed to serialize gRPC stream error event: session_id={}, error={}",
                session_id, e
            );
            // 返回包含基本信息的最小结构
            format!(
                r#"{{"session_id":"{}","message_type":"SessionPromptEnd","sub_type":"error","data":{{"code":"{}","message":"Agent computer execution error, please retry (tasks consuming too much memory may cause the agent computer process to terminate)."}}}}"#,
                session_id, error_code
            )
        }
    };

    axum::response::sse::Event::default()
        .event("error")
        .data(json_data)
}

/// 创建连接失败错误事件
///
/// 当 gRPC 连接建立失败（重试后）时发送此事件
fn create_connection_error_event(session_id: &str, message: &str) -> axum::response::sse::Event {
    let unified_message = UnifiedSessionMessage {
        session_id: session_id.to_string(),
        message_type: SessionMessageType::SessionPromptEnd,
        sub_type: "error".to_string(),
        data: serde_json::json!({
            "code": "GRPC_CONNECTION_FAILED",
            "message": message,
        }),
        timestamp: Utc::now(),
    };

    let json_data = match serde_json::to_string(&unified_message) {
        Ok(json) => json,
        Err(e) => {
            warn!(
                "⚠️ [gRPC_SSE] Failed to serialize connection error event: session_id={}, error={}",
                session_id, e
            );
            // 返回包含基本信息的最小结构
            format!(
                r#"{{"session_id":"{}","message_type":"SessionPromptEnd","sub_type":"error","data":{{"code":"GRPC_CONNECTION_FAILED","message":"Connection failed"}}}}"#,
                session_id
            )
        }
    };

    axum::response::sse::Event::default()
        .event("error")
        .data(json_data)
}

/// 将 tonic::Code 映射为业务错误码
fn map_tonic_code_to_error_code(code: Code) -> &'static str {
    match code {
        Code::Unavailable => "GRPC_SERVICE_UNAVAILABLE",
        Code::Cancelled => "GRPC_CANCELLED",
        Code::DeadlineExceeded => "GRPC_TIMEOUT",
        Code::Unknown => "GRPC_UNKNOWN_ERROR",
        _ => "GRPC_ERROR",
    }
}
