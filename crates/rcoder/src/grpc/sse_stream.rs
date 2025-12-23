//! gRPC SSE 流处理器
//!
//! 通过 gRPC SubscribeProgress 接收 agent_runner 的进度事件，
//! 并转换为 SSE 事件返回给客户端

use chrono::{DateTime, Utc};
use shared_types::grpc::{GetStatusRequest, ProgressRequest};
use shared_types::{SessionMessageType, UnifiedSessionMessage};
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
) -> impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
{
    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let session_id_clone = session_id.clone();

    // 在后台任务中处理 gRPC 流
    tokio::spawn(async move {
        info!(
            "🔗 [gRPC_SSE] 开始连接 agent_runner gRPC: addr={}, session_id={}, project_id={}",
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
                        "⚠️ [gRPC_SSE] 获取客户端失败 (尝试 {}/{}): {}, 清理连接池并重试...",
                        attempt, max_retries, e
                    );
                    pool.remove(&grpc_addr);
                    last_error_msg = format!("获取客户端失败: {}", e);
                    continue;
                }
            };

            // 🆕 2. 先检查 Agent 状态（使用 session_id 查询）
            let status_request = tonic::Request::new(GetStatusRequest {
                project_id: String::new(),            // 不使用 project_id
                session_id: session_id_clone.clone(), // 使用 session_id 查询
            });

            match client.get_status(status_request).await {
                Ok(response) => {
                    let status = response.into_inner().status;
                    if status == "idle" {
                        // Agent 闲置，发送 SessionPromptEnd 并关闭连接
                        info!(
                            "💤 [gRPC_SSE] Agent 处于闲置状态，发送 SessionPromptEnd 并关闭: session_id={}",
                            session_id_clone
                        );
                        let end_event = create_session_prompt_end_event(&session_id_clone);
                        let _ = tx.send(Ok(end_event)).await;
                        return; // 直接结束，不建立流
                    }
                    info!(
                        "🔄 [gRPC_SSE] Agent 状态为 {}, 继续建立流: session_id={}",
                        status, session_id_clone
                    );
                }
                Err(e) => {
                    // 状态检查失败，记录警告但继续尝试建立流
                    warn!(
                        "⚠️ [gRPC_SSE] Agent 状态检查失败: {}, 继续尝试建立流: session_id={}",
                        e, session_id_clone
                    );
                }
            }

            // 3. 发送 SubscribeProgress 请求
            let request = tonic::Request::new(ProgressRequest {
                session_id: session_id_clone.clone(),
            });

            match client.subscribe_progress(request).await {
                Ok(response) => {
                    info!(
                        "✅ [gRPC_SSE] 成功建立 SubscribeProgress 流: session_id={}",
                        session_id_clone
                    );

                    let mut stream = response.into_inner();

                    // 持续接收 gRPC 流中的事件
                    while let Ok(Some(progress_event)) = stream.message().await {
                        debug!(
                            "📨 [gRPC_SSE] 收到进度事件: session_id={}, message_type={}, sub_type={}",
                            session_id_clone, progress_event.message_type, progress_event.sub_type
                        );

                        // 将 ProgressEvent 转换为 SSE Event（传入 session_id 以重建完整消息结构）
                        let sse_event = progress_event_to_sse(&progress_event, &session_id_clone);

                        if tx.send(Ok(sse_event)).await.is_err() {
                            warn!(
                                "⚠️ [gRPC_SSE] 客户端已断开连接: session_id={}",
                                session_id_clone
                            );
                            // 客户端断开，直接退出任务
                            return;
                        }
                    }

                    info!("🔚 [gRPC_SSE] gRPC 流结束: session_id={}", session_id_clone);
                    // 正常结束，直接返回
                    return;
                }
                Err(e) => {
                    warn!(
                        "⚠️ [gRPC_SSE] SubscribeProgress 调用失败 (尝试 {}/{}): {}",
                        attempt, max_retries, e
                    );

                    // 如果不是最后一次尝试，清理连接池并重试
                    if attempt < max_retries {
                        info!(
                            "🔌 [gRPC_SSE] 可能是连接已断开，从连接池移除 {} 并重试...",
                            grpc_addr
                        );
                        pool.remove(&grpc_addr);
                        last_error_msg = format!("流订阅失败: {}", e);
                        continue;
                    }

                    last_error_msg = format!("流订阅最终失败: {}", e);
                }
            }
        }

        // 如果循环结束还没有 return，说明所有重试都失败了
        error!(
            "❌ [gRPC_SSE] 重试 {} 次后最终失败: session_id={}, error={}",
            max_retries, session_id_clone, last_error_msg
        );

        let error_event = axum::response::sse::Event::default()
            .event("error")
            .data(format!("gRPC 连接失败: {}", last_error_msg));
        let _ = tx.send(Ok(error_event)).await;
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
            "description": "Agent 当前无在执行任务"
        }),
        timestamp: Utc::now(),
    };

    let json_data = serde_json::to_string(&unified_message).unwrap_or_else(|_| "{}".to_string());

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
    let timestamp =
        DateTime::<Utc>::from_timestamp_millis(event.timestamp).unwrap_or_else(Utc::now);

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
    let json_data = serde_json::to_string(&unified_message).unwrap_or_else(|_| "{}".to_string());

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
                "⚠️ [gRPC_SSE] 未知的 message_type: {}, 使用 AgentSessionUpdate 作为默认值",
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
        "🔍 [CONTAINER] 获取容器 gRPC 地址: project_id={}",
        project_id
    );

    // 获取全局 DockerManager 实例
    let docker_manager = docker_manager::global::get_global_docker_manager()
        .await
        .map_err(|e| anyhow::anyhow!("获取全局 DockerManager 失败: {}", e))?;

    // 使用高级 API 获取容器信息（包含 IP）
    let agent_info = docker_manager
        .get_agent_info(project_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("未找到容器信息: project_id={}", project_id))?;

    let grpc_addr = format!("{}:{}", agent_info.container_ip, grpc_port);

    info!("✅ [CONTAINER] 获取容器 gRPC 地址: {}", grpc_addr);
    Ok(grpc_addr)
}
