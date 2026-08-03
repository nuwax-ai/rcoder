//! gRPC SSE 流处理器
//!
//! 通过 gRPC SubscribeProgress 接收 agent_runner 的进度事件，
//! 并转换为 SSE 事件返回给客户端

use chrono::{DateTime, Utc};
use shared_types::{SessionMessageType, UnifiedSessionMessage};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{info, warn};

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
    registry: Arc<crate::grpc::SessionStreamRegistry>,
    grpc_addr: String,
    session_id: String,
    pool: std::sync::Arc<crate::grpc::GrpcChannelPool>,
    locale: &'static str,
    activity_updater: Arc<dyn Fn(&str) + Send + Sync>,
    last_seq: u64,
) -> impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
{
    let (tx, rx) = tokio::sync::mpsc::channel(100);

    // 每个 HTTP SSE 请求一个客户端转发 task：共享 session 的 agent_runner 流（fan-out），
    // 按 last_seq 从 ring 增量补齐 + 订阅 broadcast 接实时，seq 去重重叠窗口。
    tokio::spawn(async move {
        // 1. 获取或创建 session 共享流（每 session 一条 agent_runner SubscribeProgress 流）
        let shared = registry
            .get_or_create(&session_id, &grpc_addr, pool, locale, activity_updater)
            .await;
        // 注册本消费者（ref_count +1）；guard drop 时 release_client（最后一个离开延迟清理共享流）
        let _guard = shared.acquire_client(Arc::clone(&registry));

        let mut client_last_seq = last_seq;
        info!(
            "🔗 [gRPC_SSE] client subscribed to shared stream: session_id={}, last_seq={}",
            session_id, client_last_seq
        );

        // 2. 先订阅 broadcast(接实时),再 replay ring(补历史)——顺序不能反!
        //    若 replay 先 subscribe 后,中间 dispatch 的事件不在 replay 也不在 receiver = 丢事件。
        //    subscribe 先 replay 后:gap 里的事件在两边都有 → dedup(seq<=client_last_seq)去重。
        let mut bc_rx = shared.subscribe();

        // 3. 增量补齐：从 ring 读取 seq > client_last_seq 的历史（断线重连补缺，不重复已收）
        for ev in shared.replay_since(client_last_seq) {
            if !forward_to_client(&tx, &ev, &session_id, &mut client_last_seq).await {
                return; // 客户端断开，或历史已含终端事件
            }
        }

        // 4. 接实时事件(dedup 跳过 replay 已发的)
        loop {
            tokio::select! {
                // HTTP 客户端断开（SSE Receiver 全 drop）→ 退出（_guard drop 时 release_client）
                _ = tx.closed() => {
                    info!(
                        "🔌 [gRPC_SSE] client disconnected: session_id={}",
                        session_id
                    );
                    return;
                }
                r = bc_rx.recv() => match r {
                    Ok(ev) => {
                        // 去重：补齐阶段已发的（补齐与订阅之间窗口）跳过；
                        // seq=0 合成消息（idle/error）无条件转发，不更新游标。
                        if ev.seq != 0 && ev.seq <= client_last_seq {
                            continue;
                        }
                        if !forward_to_client(&tx, &ev, &session_id, &mut client_last_seq).await {
                            return; // 客户端断开，或终端事件（SessionPromptEnd）
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // 慢消费者丢消息：回退 ring 按 client_last_seq 补齐缺失
                        warn!(
                            "⚠️ [gRPC_SSE] client lagged by {}, replay from ring: session_id={}",
                            n, session_id
                        );
                        for ev in shared.replay_since(client_last_seq) {
                            if !forward_to_client(&tx, &ev, &session_id, &mut client_last_seq).await {
                                return;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!(
                            "✅ [gRPC_SSE] shared stream closed: session_id={}",
                            session_id
                        );
                        return;
                    }
                }
            }
        }
    });

    tokio_stream::wrappers::ReceiverStream::new(rx)
}

/// 转发一个 ProgressEvent 到 HTTP SSE channel。
///
/// 返回 `false` 表示调用方应结束 task：HTTP 客户端断开（send 失败），或收到终端事件
/// （`SessionPromptEnd`，含 idle 的 end_turn、各类 error）。终端事件转发后必须结束 task，
/// 否则后台 task 退出后客户端会 hang（broadcast `Receiver` 不会 Closed，因为 `SharedStream`
/// 始终持有 sender）。
async fn forward_to_client(
    tx: &tokio::sync::mpsc::Sender<Result<axum::response::sse::Event, std::convert::Infallible>>,
    ev: &shared_types::grpc::ProgressEvent,
    session_id: &str,
    client_last_seq: &mut u64,
) -> bool {
    let is_terminal = ev.message_type == "SessionPromptEnd";
    // cursor-reset(#15):epoch 变化 → 重置客户端去重游标,让新 epoch 的低 seq 事件不被去重丢弃。
    if ev.message_type == "StreamReset" {
        *client_last_seq = 0;
    }
    let sse_event = progress_event_to_sse(ev, session_id);
    if tx.send(Ok(sse_event)).await.is_err() {
        return false; // HTTP 客户端断开
    }
    if ev.seq > *client_last_seq {
        *client_last_seq = ev.seq;
    }
    !is_terminal
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
    // seq>=1 时设 SSE id（=seq）：浏览器 EventSource 断线重连会自动带 `Last-Event-ID` header，
    // rcoder 据此增量补齐（只发 seq > last_seq），消除重连时的历史重复。
    let sse_event = axum::response::sse::Event::default()
        .event(&event.sub_type)
        .data(json_data);
    if event.seq > 0 {
        sse_event.id(event.seq.to_string())
    } else {
        sse_event
    }
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
        .get_container_info_by_identifier(project_id, &shared_types::ServiceType::WebAgentRunner)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get container info: {}", e))?
        .ok_or_else(|| anyhow::anyhow!("Container info not found: project_id={}", project_id))?;

    let grpc_addr = format!("{}:{}", agent_info.container_ip, grpc_port);

    info!("[CONTAINER] get container gRPC addr: {}", grpc_addr);
    Ok(grpc_addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(message_type: &str, seq: u64) -> shared_types::grpc::ProgressEvent {
        shared_types::grpc::ProgressEvent {
            message_type: message_type.to_string(),
            sub_type: "test".to_string(),
            payload: "{}".to_string(),
            request_id: None,
            seq,
            timestamp: 0,
        }
    }

    #[tokio::test]
    async fn forward_to_client_continues_for_non_terminal_event() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<
            Result<axum::response::sse::Event, std::convert::Infallible>,
        >(10);
        let mut last_seq = 0_u64;
        let ev = make_event("AgentSessionUpdate", 5);

        let cont = forward_to_client(&tx, &ev, "s1", &mut last_seq).await;
        assert!(cont, "non-terminal event should continue");
        assert_eq!(last_seq, 5, "seq should advance");
        assert!(rx.recv().await.is_some(), "event should be sent to client");
    }

    #[tokio::test]
    async fn forward_to_client_stops_on_terminal_event() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<
            Result<axum::response::sse::Event, std::convert::Infallible>,
        >(10);
        let mut last_seq = 10_u64;
        let ev = make_event("SessionPromptEnd", 0); // 终端 + seq=0 合成消息

        let cont = forward_to_client(&tx, &ev, "s1", &mut last_seq).await;
        assert!(
            !cont,
            "SessionPromptEnd must stop the forward task (avoid hang)"
        );
        assert_eq!(last_seq, 10, "seq=0 must not advance last_seq");
    }

    #[tokio::test]
    async fn forward_to_client_stops_when_client_disconnected() {
        let (tx, rx) = tokio::sync::mpsc::channel::<
            Result<axum::response::sse::Event, std::convert::Infallible>,
        >(10);
        drop(rx); // 模拟 HTTP 客户端断开
        let mut last_seq = 0_u64;
        let ev = make_event("AgentSessionUpdate", 5);

        let cont = forward_to_client(&tx, &ev, "s1", &mut last_seq).await;
        assert!(!cont, "send failure (client gone) must stop the task");
    }
}
