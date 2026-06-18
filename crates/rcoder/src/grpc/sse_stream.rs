//! gRPC SSE 流处理器
//!
//! 通过 gRPC SubscribeProgress 接收 agent_runner 的进度事件，
//! 并转换为 SSE 事件返回给客户端

use chrono::{DateTime, Utc};
use shared_types::grpc::{GetStatusRequest, ProgressRequest};
use shared_types::{SessionMessageType, UnifiedSessionMessage};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use tonic::Code;
use tracing::{debug, error, info, warn};

/// 活跃时间更新节流间隔（秒）
///
/// 任务进度事件可能高频到达（agent_message_chunk 每 10ms 一条），如果每条都
/// 打 storage 写锁会拖累热路径。10s 节流意味着每 10s 最多更新一次 last_activity，
/// 对于 cleanup_task 的 30min idle_timeout 来说足够精细。
const ACTIVITY_UPDATE_THROTTLE_SECS: i64 = 10;

/// 创建基于 gRPC 的 SSE 代理流
///
/// 通过 gRPC `SubscribeProgress` 方法订阅 agent_runner 的进度事件，
/// 并将事件转换为 SSE 格式返回
///
/// 🚀 优化：使用连接池 + 智能重试机制
/// 🆕 新增：在建立流之前检查 Agent 状态，如果 Agent 闲置则直接发送 SessionPromptEnd 并关闭
///
/// ## Bug 5 修复：活跃时间更新
///
/// 收到 agent 任务进度事件（非心跳）时，节流（10s 一次）更新 project + container 活跃时间，
/// 防止 cleanup_task 在 agent 长任务执行期间误判 idle 并销毁容器。
///
/// 节流规则：
/// - `Heartbeat` 消息：不更新（心跳只代表连接活着，不代表用户在用）
/// - 其他消息（SessionPromptStart/End、AgentSessionUpdate、AcpRequestPermission 等）：可更新
/// - 距上次更新 < 10s：跳过本次更新
///
/// ## 关于 activity_updater 闭包参数
///
/// 不直接传 `Arc<AppState>` 是因为 rcoder 同时作为 lib 和 bin 编译，
/// `crate::router::AppState` 在两边是不同的类型实例。改用闭包解耦：
/// 调用方在 lib 内部捕获 state 引用，bin crate 不需要知道 AppState 类型。
pub async fn create_grpc_sse_stream(
    grpc_addr: String,
    session_id: String,
    project_id: String,
    pool: std::sync::Arc<crate::grpc::GrpcChannelPool>,
    locale: &'static str,
    activity_updater: Arc<dyn Fn(&str) + Send + Sync>,
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

        // 节流状态：上次活跃时间更新的 Unix 秒（0 表示从未更新过）
        // AtomicI64 无锁，跨 await 安全
        let last_activity_update_secs: AtomicI64 = AtomicI64::new(0);

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
                    pool.remove(&grpc_addr).await;
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
                    //
                    // M4 修复：用 tokio::select! 监听 tx.closed() 实现客户端断开的早期检测。
                    // 原实现只能在下次 send 失败时才知道断开，期间浪费 CPU 接收/转换 gRPC 事件。
                    loop {
                        tokio::select! {
                            // 客户端断开（所有 Receiver 都 drop）：立即停止接收 gRPC
                            _ = tx.closed() => {
                                info!(
                                    "🔌 [gRPC_SSE] Client disconnected (early detection), stopping gRPC stream: session_id={}",
                                    session_id_clone
                                );
                                return;
                            }
                            // gRPC 流消息
                            msg = stream.message() => match msg {
                                Ok(Some(progress_event)) => {
                                    debug!(
                                        "📨 [gRPC_SSE] Received progress event: session_id={}, message_type={}, sub_type={}, payload={}",
                                        session_id_clone,
                                        progress_event.message_type,
                                        progress_event.sub_type,
                                        if progress_event.payload.len() > 2000 {
                                            let truncated: String = progress_event.payload.chars().take(2000).collect();
                                            format!("{}... (truncated)", truncated)
                                        } else {
                                            progress_event.payload.clone()
                                        }
                                    );

                                    // Bug 5 修复：非心跳事件节流更新活跃时间
                                    // Heartbeat 只是保活，不代表用户在用；其他事件代表 agent 在执行任务
                                    if progress_event.message_type != "Heartbeat" {
                                        maybe_update_session_activity(
                                            &activity_updater,
                                            &session_id_clone,
                                            &last_activity_update_secs,
                                        );
                                    }

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
                        pool.remove(&grpc_addr).await;
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
        "AcpRequestPermission" => SessionMessageType::AcpRequestPermission,
        "Heartbeat" => SessionMessageType::Heartbeat,
        // 默认作为 AgentSessionUpdate 处理
        _ => {
            warn!(
                "⚠️ [gRPC_SSE] Unknown message_type '{}', falling back to AgentSessionUpdate",
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
pub async fn get_container_grpc_addr(
    runtime: &Arc<dyn container_runtime_api::ContainerRuntime>,
    project_id: &str,
    grpc_port: u16,
) -> anyhow::Result<String> {
    info!(
        "🔍 [CONTAINER] Getting container gRPC address: project_id={}",
        project_id
    );

    let agent_info = runtime
        .get_container_info_by_identifier(project_id, &shared_types::ServiceType::RCoder)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get container info: {}", e))?
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
    _message: &str,
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

/// 节流更新 session 活跃时间（Bug 5 修复）
///
/// - 距上次更新 ≥ `ACTIVITY_UPDATE_THROTTLE_SECS`（10s）才真正调用 updater
/// - updater 由调用方提供（通常封装 `state.update_session_activity`）
/// - updater 失败不阻断 SSE 流
fn maybe_update_session_activity(
    activity_updater: &Arc<dyn Fn(&str) + Send + Sync>,
    session_id: &str,
    last_update_secs: &AtomicI64,
) {
    let now_secs = Utc::now().timestamp();
    let last = last_update_secs.load(Ordering::Relaxed);
    if now_secs - last < ACTIVITY_UPDATE_THROTTLE_SECS {
        // 节流窗口内，跳过
        return;
    }

    // CAS：只有一个并发请求能通过（其他会被下次 load 拦下）
    // 失败说明其他线程刚刚更新过，跳过即可
    if last_update_secs
        .compare_exchange(last, now_secs, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    // 调用 updater（同时更新 project + container 活跃时间）
    // 函数定义在 adapter.rs，原本是死代码，这里接上调用点（Issue 5 一并修复）
    activity_updater(session_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_atomic_i64_initial_value() {
        let counter = AtomicI64::new(0);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_cas_update_semantics() {
        let counter = AtomicI64::new(100);
        // CAS 成功
        let result = counter.compare_exchange(100, 200, Ordering::AcqRel, Ordering::Acquire);
        assert!(result.is_ok());
        assert_eq!(counter.load(Ordering::Relaxed), 200);

        // CAS 失败（期望值不匹配）
        let result = counter.compare_exchange(100, 300, Ordering::AcqRel, Ordering::Acquire);
        assert!(result.is_err());
        assert_eq!(counter.load(Ordering::Relaxed), 200);
    }
}
