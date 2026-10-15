//! gRPC SSE 流处理器
//!
//! 通过 gRPC SubscribeProgress 接收 agent_runner 的进度事件，
//! 并转换为 SSE 事件返回给客户端

use chrono::{DateTime, Utc};

use super::session_stream_registry::{
    MAX_RETRIES, is_stream_closing, is_turn_terminal, make_cursor_reset_event,
    make_prompt_end_event, make_stream_error_event, make_terminal_error_event,
    maybe_update_activity,
};
use shared_types::{SessionMessageType, UnifiedSessionMessage};
use std::sync::Arc;
use tracing::{Instrument, error, info, warn};

// hotpath 通道观测稳定别名：hotpath feature（经 feature 统一）开启时是插桩
// wrapper，关闭时即原生 tokio 类型——类型由依赖自身的 feature 决定，禁止按
// 本 crate cfg 手工切换（specs T2.1 裁定）。
use hotpath::wrap::tokio::sync::mpsc as obs_mpsc;

/// A failed read has its own captured origin. A later retry or diagnostic
/// observation cannot replace its phase, cause or request identity.
struct CapturedFailure {
    code: tonic::Code,
    cause: String,
    stage: &'static str,
    request_id: Option<String>,
}

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
/// `crate::app_state::AppState` 在两边是不同的类型实例。改用闭包解耦：
/// 调用方在 lib 内部捕获 state 引用，bin crate 不需要知道 AppState 类型。
// Hotpath 埋点：SSE 订阅建立时延（async fn spawn 转发任务后即返回，不覆盖订阅生命周期；feature 关闭时 no-op）
#[hotpath::measure]
#[allow(clippy::too_many_arguments)] // SSE 流构建本质多参;diag_ctx 为新增诊断上下文
pub async fn create_grpc_sse_stream(
    registry: Arc<crate::grpc::SessionStreamRegistry>,
    grpc_addr: String,
    session_id: String,
    pool: Arc<crate::grpc::GrpcChannelPool>,
    locale: &'static str,
    activity_updater: Arc<dyn Fn(&str) + Send + Sync>,
    diag_ctx: Option<Arc<crate::utils::DiagCtx>>,
    last_seq: u64,
) -> impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
{
    // 容量 100 不变；hotpath channel! 关闭时原样返回原生通道（零插桩），
    // 开启时包装 Sender/Receiver 端点（send/recv 记排队延迟 delay）。
    // wrapper 重建内部通道，表达式必须内联构造（clone 先于包装的端点会脱管）。
    let (tx, mut rx): (obs_mpsc::Sender<_>, obs_mpsc::Receiver<_>) =
        hotpath::channel!(tokio::sync::mpsc::channel(100), label = "sse_client_events");

    // SSE 在线订阅 gauge（RAII：spawn 的任务被 abort / 任何 return 路径都会 -1）
    struct SubscriptionGuard;
    impl SubscriptionGuard {
        fn new() -> Self {
            rcoder_telemetry::prometheus::inc_sse_subscription();
            SubscriptionGuard
        }
    }
    impl Drop for SubscriptionGuard {
        fn drop(&mut self) {
            rcoder_telemetry::prometheus::dec_sse_subscription();
        }
    }

    // Register before spawning, so shutdown also owns a task not yet polled.
    if let Some(task_guard) = registry.admit_stream(&grpc_addr, &session_id) {
        let cancel = task_guard.token();
        let sse_span =
            tracing::info_span!("sse_subscribe", session_id = %session_id, addr = %grpc_addr);
        let panic_tx = tx.clone();
        let panic_sid = session_id.clone();
        let panic_registry = Arc::clone(&registry);
        rcoder_obs::spawn(async move {
            // Guard covers the real task lifetime, including panic finalization.
            let _task_guard = task_guard;
            let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(async move {
                let _subscription = SubscriptionGuard::new();
                // Connecting, subscribing, retry cache eviction and diagnosis
                // are reads. All respond to shutdown or client disconnect.
                macro_rules! cancellable_read {
                    ($future:expr) => {
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => return,
                            _ = tx.closed() => return,
                            result = $future => result,
                        }
                    };
                }
                if cancel.is_cancelled() || tx.is_closed() {
                    return;
                }
                let is_first_client = registry.claim_first_client(&session_id);
                let mut client_last_seq = last_seq;
                let initial_from = if last_seq > 0 { last_seq } else if is_first_client { 0 } else { u64::MAX };
                info!(%session_id, client_last_seq, is_first_client, initial_from, "gRPC SSE client stream started");
                let activity_secs = std::sync::atomic::AtomicI64::new(0);
                let mut from_seq = initial_from;
                // This identity belongs to the event observed on this stream;
                // cancellation ends it, and no unrelated latest chat is queried.
                let mut active_request_id = None;
                let mut captured_failure: Option<CapturedFailure> = None;
                for attempt in 1..=MAX_RETRIES {
                    let mut client = match cancellable_read!(pool.get_client(&grpc_addr)) {
                        Ok(client) => client,
                        Err(error) => {
                            let cause = shared_types::sanitize_error_text(&format!("{error:#}"));
                            warn!(%session_id, attempt, %cause, "gRPC SSE connect failed");
                            let captured = captured_failure.get_or_insert_with(|| CapturedFailure {
                                code: tonic::Code::Unavailable, cause, stage: "grpc_connect",
                                request_id: active_request_id.clone(),
                            });
                            cancellable_read!(pool.remove(&grpc_addr));
                            if attempt < MAX_RETRIES { continue; }
                            let event = cancellable_read!(make_terminal_error_event(
                                diag_ctx.as_ref(), locale, captured.code, &captured.cause,
                                captured.stage, captured.request_id.clone(),
                            ));
                            let _ = forward_to_client(&tx, &event, &session_id, &mut client_last_seq).await;
                            registry.release_first_client_claim(&session_id);
                            return;
                        }
                    };
                    let request = crate::grpc::locale_metadata::new_request_with_locale(
                        shared_types::grpc::ProgressRequest {
                            session_id: session_id.clone(), from_seq: Some(from_seq),
                        }, locale,
                    );
                    let mut stream = match cancellable_read!(client.subscribe_progress(request)) {
                        Ok(response) => response.into_inner(),
                        Err(error) => {
                            let cause = shared_types::sanitize_error_text(error.message());
                            warn!(%session_id, attempt, %cause, "gRPC SSE subscribe failed");
                            let captured = captured_failure.get_or_insert_with(|| CapturedFailure {
                                code: error.code(), cause, stage: "grpc_subscribe",
                                request_id: active_request_id.clone(),
                            });
                            if attempt < MAX_RETRIES {
                                cancellable_read!(pool.remove(&grpc_addr));
                                continue;
                            }
                            let event = cancellable_read!(make_terminal_error_event(
                                diag_ctx.as_ref(), locale, captured.code, &captured.cause,
                                captured.stage, captured.request_id.clone(),
                            ));
                            let _ = forward_to_client(&tx, &event, &session_id, &mut client_last_seq).await;
                            registry.release_first_client_claim(&session_id);
                            return;
                        }
                    };
                    info!(%session_id, from_seq, "gRPC SSE SubscribeProgress established");
                    captured_failure = None;
                    loop {
                        match cancellable_read!(stream.message()) {
                            Ok(Some(event)) => {
                                maybe_update_activity(&activity_updater, &session_id, &activity_secs);
                                if let Some(request_id) = &event.request_id {
                                    active_request_id = Some(request_id.clone());
                                }
                                if event.seq != 0 && event.seq <= client_last_seq {
                                    warn!(%session_id, seq = event.seq, cursor = client_last_seq, "gRPC SSE sequence regressed; resetting cursor");
                                    let reset = make_cursor_reset_event();
                                    if !forward_to_client(&tx, &reset, &session_id, &mut client_last_seq).await { return; }
                                }
                                let keep_open = forward_to_client(&tx, &event, &session_id, &mut client_last_seq).await;
                                if event.message_type == "SessionPromptEnd" && event.sub_type == "cancelled" {
                                    active_request_id = None;
                                }
                                if !keep_open {
                                    if is_turn_terminal(&event.message_type, &event.sub_type) {
                                        registry.release_first_client_claim(&session_id);
                                    }
                                    return;
                                }
                            }
                            Ok(None) => {
                                let mut event = make_prompt_end_event();
                                event.request_id = active_request_id.clone();
                                let _ = forward_to_client(&tx, &event, &session_id, &mut client_last_seq).await;
                                registry.release_first_client_claim(&session_id);
                                return;
                            }
                            Err(error) => {
                                let cause = shared_types::sanitize_error_text(error.message());
                                warn!(%session_id, code = %error.code(), %cause, "gRPC SSE stream failed");
                                let captured = captured_failure.get_or_insert_with(|| CapturedFailure {
                                    code: error.code(), cause, stage: "grpc_stream",
                                    request_id: active_request_id.clone(),
                                });
                                if attempt < MAX_RETRIES {
                                    cancellable_read!(pool.remove(&grpc_addr));
                                    from_seq = client_last_seq;
                                    break;
                                }
                                let event = super::session_stream_registry::make_stream_error_event_with_identity(
                                    captured.code, &captured.cause, locale, captured.stage, captured.request_id.clone(),
                                );
                                let _ = forward_to_client(&tx, &event, &session_id, &mut client_last_seq).await;
                                registry.release_first_client_claim(&session_id);
                                return;
                            }
                        }
                    }
                }
            })).await;
            if outcome.is_err() {
                error!(%panic_sid, "gRPC SSE forwarding task panicked");
                let event = make_stream_error_event(tonic::Code::Internal, "forward task panicked");
                drop(panic_tx.try_send(Ok(progress_event_to_sse(&event, &panic_sid))));
                panic_registry.release_first_client_claim(&panic_sid);
            }
        }.instrument(sse_span));
    }

    // wrapper Receiver 非 tokio 原生类型，ReceiverStream 不再适用；poll_fn 薄适配
    // 逐次委托 poll_recv：队满背压、关闭（None 终态）、游标与 panic 语义与
    // ReceiverStream 一致（两者 poll_next 实现同为 poll_recv）。
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

/// 转发一个 ProgressEvent 到 HTTP SSE channel。
///
/// 返回 `false` 表示调用方应结束 task：HTTP 客户端断开（send 失败），或收到终端事件
/// （`SessionPromptEnd`，含 idle 的 end_turn、各类 error）。终端事件转发后必须结束 task，
/// 否则后台 task 退出后客户端会 hang（broadcast `Receiver` 不会 Closed，因为 `SharedStream`
/// 始终持有 sender）。
async fn forward_to_client(
    tx: &obs_mpsc::Sender<Result<axum::response::sse::Event, std::convert::Infallible>>,
    ev: &shared_types::grpc::ProgressEvent,
    session_id: &str,
    client_last_seq: &mut u64,
) -> bool {
    // 关流判定：turn 终态（end_turn/error）+ rcoder 合成的 stream_ended。
    // cancelled 不关流——它是"用户连发消息自动取消"的常态事件，agent 随后继续
    // 执行新任务，流保持供下一轮实时投递（与 agent_runner 侧订阅判定对齐）。
    let is_terminal = is_stream_closing(&ev.message_type, &ev.sub_type);
    // cursor-reset(#15):epoch 变化 → 重置客户端去重游标,让新 epoch 的低 seq 事件不被去重丢弃。
    if ev.message_type == "StreamReset" {
        *client_last_seq = 0;
    }
    let sse_event = progress_event_to_sse(ev, session_id);
    if let Err(error) = tx.try_send(Ok(sse_event)) {
        warn!(%session_id, %error, "Closing SSE subscriber because its queue is full or closed");
        return false;
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
    let mut data: serde_json::Value =
        serde_json::from_str(&event.payload).unwrap_or(serde_json::Value::Null);
    // The proto identity is captured from the original event. Keep the existing
    // data.request_id wire contract without fabricating a session/task identity.
    if let Some(request_id) = &event.request_id
        && let Some(object) = data.as_object_mut()
    {
        object.insert(
            "request_id".into(),
            serde_json::Value::String(request_id.clone()),
        );
    }
    if is_turn_terminal(&event.message_type, &event.sub_type) && event.sub_type == "error" {
        // Error fields may originate in ACP or gRPC. Do not alter normal agent
        // output, but scrub diagnostic strings before emitting them publicly.
        sanitize_diagnostic_fields(&mut data);
    }

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
            serde_json::json!({
                "sessionId": session_id, "messageType": "agentSessionUpdate",
                "subType": event.sub_type, "data": null,
            })
            .to_string()
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

fn sanitize_diagnostic_fields(data: &mut serde_json::Value) {
    if let serde_json::Value::Object(fields) = data {
        for (key, value) in fields {
            if matches!(
                key.as_str(),
                "message" | "error_message" | "detail" | "hint" | "observed_container_state"
            ) {
                if let Some(text) = value.as_str() {
                    *value = serde_json::Value::String(shared_types::sanitize_error_text(text));
                }
            } else if key == "error_detail" {
                sanitize_diagnostic_fields(value);
            }
        }
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

    #[tokio::test(flavor = "current_thread")]
    async fn sse_shutdown_registers_subscription_before_spawn_is_polled() {
        let registry = Arc::new(crate::grpc::SessionStreamRegistry::new());
        let stream = create_grpc_sse_stream(
            registry.clone(),
            "127.0.0.1:1".into(),
            "session-original".into(),
            Arc::new(crate::grpc::GrpcChannelPool::new()),
            "en-US",
            Arc::new(|_| {}),
            None,
            0,
        )
        .await;
        // No scheduler yield occurred: shutdown must already see the accepted
        // task rather than rely on its first poll to register ownership.
        assert_eq!(
            registry.len(),
            1,
            "accepted SSE was invisible before spawn polling"
        );
        assert!(registry.shutdown_session("session-original"));
        drop(stream);
    }

    #[tokio::test]
    async fn sse_shutdown_cancels_pending_grpc_initialization() {
        use futures_util::StreamExt as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let registry = Arc::new(crate::grpc::SessionStreamRegistry::new());
        let stream = create_grpc_sse_stream(
            registry.clone(),
            addr,
            "session-original".into(),
            Arc::new(crate::grpc::GrpcChannelPool::new()),
            "en-US",
            Arc::new(|_| {}),
            None,
            0,
        )
        .await;
        // Controlled transport holds the connection without responding to H2
        // or SubscribeProgress. Cancellation must interrupt initialization.
        let (_held_connection, _) =
            tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept())
                .await
                .unwrap()
                .unwrap();
        assert!(registry.shutdown_session("session-original"));
        tokio::pin!(stream);
        let ended =
            tokio::time::timeout(std::time::Duration::from_millis(500), stream.next()).await;
        assert!(
            ended.is_ok(),
            "SSE initialization ignored shutdown cancellation"
        );
        assert!(
            ended.unwrap().is_none(),
            "shutdown must close the response stream"
        );
    }

    fn make_event(message_type: &str, seq: u64) -> shared_types::grpc::ProgressEvent {
        make_prompt_end(message_type, "test", seq)
    }

    fn make_prompt_end(
        message_type: &str,
        sub_type: &str,
        seq: u64,
    ) -> shared_types::grpc::ProgressEvent {
        shared_types::grpc::ProgressEvent {
            message_type: message_type.to_string(),
            sub_type: sub_type.to_string(),
            payload: "{}".to_string(),
            request_id: None,
            seq,
            timestamp: 0,
        }
    }

    #[tokio::test]
    async fn full_subscriber_does_not_block_forwarder() {
        // 测试通道同样经 channel! 构造：hotpath 开启时与生产同型（wrapper），
        // 关闭时原样返回原生通道——避免 feature 统一后测试与签名错配。
        let (tx, _rx) = hotpath::channel!(tokio::sync::mpsc::channel(1));
        tx.try_send(Ok(axum::response::sse::Event::default().data("occupied")))
            .unwrap();
        let ev = shared_types::grpc::ProgressEvent::default();
        let mut last_seq = 0;
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            forward_to_client(&tx, &ev, "slow", &mut last_seq),
        )
        .await;
        assert!(
            result.is_ok(),
            "full subscriber must not prevent forwarder cancellation/exit"
        );
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn forward_to_client_continues_for_non_terminal_event() {
        let (tx, mut rx) = hotpath::channel!(tokio::sync::mpsc::channel::<
            Result<axum::response::sse::Event, std::convert::Infallible>,
        >(10));
        let mut last_seq = 0_u64;
        let ev = make_event("AgentSessionUpdate", 5);

        let cont = forward_to_client(&tx, &ev, "s1", &mut last_seq).await;
        assert!(cont, "non-terminal event should continue");
        assert_eq!(last_seq, 5, "seq should advance");
        assert!(rx.recv().await.is_some(), "event should be sent to client");
    }

    #[tokio::test]
    async fn forward_to_client_stops_on_terminal_event() {
        let (tx, _rx) = hotpath::channel!(tokio::sync::mpsc::channel::<
            Result<axum::response::sse::Event, std::convert::Infallible>,
        >(10));
        let mut last_seq = 10_u64;
        let ev = make_prompt_end("SessionPromptEnd", "end_turn", 0); // turn 终态 + seq=0 合成消息

        let cont = forward_to_client(&tx, &ev, "s1", &mut last_seq).await;
        assert!(
            !cont,
            "SessionPromptEnd must stop the forward task (avoid hang)"
        );
        assert_eq!(last_seq, 10, "seq=0 must not advance last_seq");
    }

    #[tokio::test]
    async fn forward_to_client_stops_when_client_disconnected() {
        let (tx, rx) = hotpath::channel!(tokio::sync::mpsc::channel::<
            Result<axum::response::sse::Event, std::convert::Infallible>,
        >(10));
        drop(rx); // 模拟 HTTP 客户端断开
        let mut last_seq = 0;
        let ev = make_event("AgentSessionUpdate", 5);

        let cont = forward_to_client(&tx, &ev, "s1", &mut last_seq).await;
        assert!(!cont, "send failure (client gone) must stop the task");
    }

    /// seq>=1 的真实事件必须带 SSE `id` 行（=seq）——浏览器 EventSource 断线
    /// 自动重连凭此回传 Last-Event-ID，服务端才能只补增量不重放。
    /// 走真实序列化路径（IntoResponse → body 文本）断言最终 wire 格式。
    #[tokio::test]
    async fn progress_event_to_sse_sets_id_for_real_seq() {
        let ev = make_event("AgentSessionUpdate", 42);
        let sse_event = progress_event_to_sse(&ev, "s1");
        let text = render_sse_event(sse_event).await;
        assert!(
            text.contains("id: 42"),
            "SSE wire format must carry id line, got: {text}"
        );
    }

    /// seq=0 的合成消息（idle/error 哨兵）不设 id——0 是"无游标"哨兵语义，
    /// 不能当作真实事件编号回传给客户端。
    #[tokio::test]
    async fn progress_event_to_sse_omits_id_for_zero_seq() {
        let ev = make_event("SessionPromptEnd", 0);
        let sse_event = progress_event_to_sse(&ev, "s1");
        let text = render_sse_event(sse_event).await;
        assert!(
            !text.contains("id:"),
            "synthetic seq=0 event must not carry id line, got: {text}"
        );
    }

    /// 单事件经 axum SSE 序列化为 wire 文本（测试辅助）
    async fn render_sse_event(event: axum::response::sse::Event) -> String {
        use axum::response::IntoResponse;
        use http_body_util::BodyExt;

        let resp = axum::response::Sse::new(futures_util::stream::once(async move {
            Ok::<_, std::convert::Infallible>(event)
        }))
        .into_response();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .expect("collect sse body")
            .to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn forward_to_client_continues_on_cancelled() {
        // cancelled = 用户连发消息自动取消当前任务：常态事件，不关流，
        // 流保留给随后的新任务实时投递
        let (tx, _rx) = hotpath::channel!(tokio::sync::mpsc::channel::<
            Result<axum::response::sse::Event, std::convert::Infallible>,
        >(10));
        let mut last_seq = 10_u64;
        let ev = make_prompt_end("SessionPromptEnd", "cancelled", 11);

        let cont = forward_to_client(&tx, &ev, "s", &mut last_seq).await;
        assert!(cont, "cancelled must NOT close the stream");
        assert_eq!(last_seq, 11);
    }

    #[tokio::test]
    async fn forward_to_client_stops_on_error() {
        let (tx, _rx) = hotpath::channel!(tokio::sync::mpsc::channel::<
            Result<axum::response::sse::Event, std::convert::Infallible>,
        >(10));
        let mut last_seq = 10_u64;
        let ev = make_prompt_end("SessionPromptEnd", "error", 0);

        let cont = forward_to_client(&tx, &ev, "s", &mut last_seq).await;
        assert!(!cont, "error is a turn terminal and must close the stream");
    }

    #[tokio::test]
    async fn forward_to_client_stops_on_stream_ended() {
        // rcoder 合成的流替换信号必须关流，否则客户端转发 task hang
        let (tx, _rx) = hotpath::channel!(tokio::sync::mpsc::channel::<
            Result<axum::response::sse::Event, std::convert::Infallible>,
        >(10));
        let mut last_seq = 10_u64;
        let ev = make_prompt_end("SessionPromptEnd", "stream_ended", 0);

        let cont = forward_to_client(&tx, &ev, "s", &mut last_seq).await;
        assert!(
            !cont,
            "stream_ended must close the stream (client should reconnect)"
        );
    }
}

#[cfg(test)]
#[path = "sse_stream/protocol_tests.rs"]
mod protocol_tests;
