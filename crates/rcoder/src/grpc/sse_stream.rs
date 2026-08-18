//! gRPC SSE 流处理器
//!
//! 通过 gRPC SubscribeProgress 接收 agent_runner 的进度事件，
//! 并转换为 SSE 事件返回给客户端

use chrono::{DateTime, Utc};
use shared_types::{SessionMessageType, UnifiedSessionMessage};
use std::sync::Arc;
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
#[allow(clippy::too_many_arguments)] // SSE 流构建本质多参;diag_ctx 为新增诊断上下文
pub async fn create_grpc_sse_stream(
    registry: Arc<crate::grpc::SessionStreamRegistry>,
    grpc_addr: String,
    session_id: String,
    pool: Arc<crate::grpc::GrpcChannelPool>,
    locale: &'static str,
    activity_updater: Arc<dyn Fn(&str) + Send + Sync>,
    diag_ctx: Option<Arc<crate::handler::utils::DiagCtx>>,
    last_seq: u64,
) -> impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
{
    let (tx, rx) = tokio::sync::mpsc::channel(100);

    // 每个 HTTP SSE 请求一条独立的 agent_runner SubscribeProgress 订阅（纯转发，
    // agent_runner 唯一真源）：回放由订阅参数表达——带游标增量 / 首连全量兜
    // chat→SSE 时间差 / 中间连接 live-only（不重放，防重复红线）。
    tokio::spawn(async move {
        let is_first_client = registry.claim_first_client(&session_id);
        let mut client_last_seq = last_seq;
        let initial_from = if last_seq > 0 {
            last_seq
        } else if is_first_client {
            0
        } else {
            u64::MAX
        };
        info!(
            "🔗 [gRPC_SSE] client stream started: session_id={}, last_seq={}, first_client={}, from_seq={}",
            session_id, client_last_seq, is_first_client, initial_from
        );

        // 活跃登记：容器销毁路径（reaper/destroyer）按 addr/session 取消本 task
        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        registry.register_stream(&grpc_addr, &session_id, &cancel);

        let activity_secs = std::sync::atomic::AtomicI64::new(0);
        let mut from_seq = initial_from;

        for attempt in 1..=crate::grpc::session_stream_registry::MAX_RETRIES {
            if cancel.is_cancelled() {
                info!(
                    "[gRPC_SSE] cancelled (container shutdown): session_id={}",
                    session_id
                );
                return;
            }
            let mut client = match pool.get_client(&grpc_addr).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(
                        "[gRPC_SSE] get_client failed (attempt {}/{}): session_id={}, {}",
                        attempt,
                        crate::grpc::session_stream_registry::MAX_RETRIES,
                        session_id,
                        e
                    );
                    pool.remove(&grpc_addr).await;
                    if attempt < crate::grpc::session_stream_registry::MAX_RETRIES {
                        continue;
                    }
                    let err_ev = crate::grpc::session_stream_registry::make_terminal_error_event(
                        diag_ctx.as_ref(),
                        locale,
                    )
                    .await;
                    let _ =
                        forward_to_client(&tx, &err_ev, &session_id, &mut client_last_seq).await;
                    registry.release_first_client_claim(&session_id);
                    return;
                }
            };

            let req = crate::grpc::locale_metadata::new_request_with_locale(
                shared_types::grpc::ProgressRequest {
                    session_id: session_id.clone(),
                    from_seq: Some(from_seq),
                },
                locale,
            );
            let mut stream = match client.subscribe_progress(req).await {
                Ok(resp) => resp.into_inner(),
                Err(e) => {
                    warn!(
                        "[gRPC_SSE] subscribe failed (attempt {}/{}): session_id={}, {}",
                        attempt,
                        crate::grpc::session_stream_registry::MAX_RETRIES,
                        session_id,
                        e
                    );
                    if attempt < crate::grpc::session_stream_registry::MAX_RETRIES {
                        pool.remove(&grpc_addr).await;
                        continue;
                    }
                    let err_ev = crate::grpc::session_stream_registry::make_terminal_error_event(
                        diag_ctx.as_ref(),
                        locale,
                    )
                    .await;
                    let _ =
                        forward_to_client(&tx, &err_ev, &session_id, &mut client_last_seq).await;
                    registry.release_first_client_claim(&session_id);
                    return;
                }
            };
            info!(
                "[gRPC_SSE] SubscribeProgress established: session_id={}, from_seq={}",
                session_id, from_seq
            );

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        info!("[gRPC_SSE] cancelled (container shutdown): session_id={}", session_id);
                        return;
                    }
                    _ = tx.closed() => {
                        info!("🔌 [gRPC_SSE] client disconnected: session_id={}", session_id);
                        return;
                    }
                    msg = stream.message() => match msg {
                        Ok(Some(ev)) => {
                            crate::grpc::session_stream_registry::maybe_update_activity(
                                &activity_updater,
                                &session_id,
                                &activity_secs,
                            );
                            // seq 回退检测（替代旧 GetStatus/epoch 轮询）：agent_runner
                            // 重启后新 epoch 从 1 重新计数，旧游标会把新事件当重复丢弃——
                            // 先发 cursor-reset 哨兵并清零本地游标
                            if ev.seq != 0 && ev.seq <= client_last_seq {
                                warn!(
                                    "⚠️ [gRPC_SSE] seq regression (agent restarted?): session_id={}, seq={}, cursor={}, resetting",
                                    session_id, ev.seq, client_last_seq
                                );
                                let reset = crate::grpc::session_stream_registry::make_cursor_reset_event();
                                if !forward_to_client(&tx, &reset, &session_id, &mut client_last_seq).await {
                                    return;
                                }
                            }
                            if !forward_to_client(&tx, &ev, &session_id, &mut client_last_seq).await {
                                // turn 终态（end_turn/error）→ 归还首连资格（新一轮 turn
                                // 的首连重新可兜时间差）；客户端断开 → 直接退出
                                if crate::grpc::session_stream_registry::is_turn_terminal(
                                    &ev.message_type, &ev.sub_type,
                                ) {
                                    registry.release_first_client_claim(&session_id);
                                }
                                return;
                            }
                        }
                        Ok(None) => {
                            // agent_runner 正常关流但未推终端（防御）：补终态避免客户端 hang
                            let ev = crate::grpc::session_stream_registry::make_prompt_end_event();
                            let _ = forward_to_client(&tx, &ev, &session_id, &mut client_last_seq).await;
                            registry.release_first_client_claim(&session_id);
                            return;
                        }
                        Err(e) => {
                            warn!(
                                "[gRPC_SSE] stream error: session_id={}, code={}, msg={}",
                                session_id, e.code(), e.message()
                            );
                            if attempt < crate::grpc::session_stream_registry::MAX_RETRIES {
                                pool.remove(&grpc_addr).await;
                                from_seq = client_last_seq; // 增量重订
                                break; // 内层退出，外层重试
                            }
                            let err_ev = crate::grpc::session_stream_registry::make_stream_error_event(
                                e.code(), e.message(),
                            );
                            let _ = forward_to_client(&tx, &err_ev, &session_id, &mut client_last_seq).await;
                            registry.release_first_client_claim(&session_id);
                            return;
                        }
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
    // 关流判定：turn 终态（end_turn/error）+ rcoder 合成的 stream_ended。
    // cancelled 不关流——它是"用户连发消息自动取消"的常态事件，agent 随后继续
    // 执行新任务，流保持供下一轮实时投递（与 agent_runner 侧订阅判定对齐）。
    let is_terminal =
        super::session_stream_registry::is_stream_closing(&ev.message_type, &ev.sub_type);
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
        let (tx, rx) = tokio::sync::mpsc::channel::<
            Result<axum::response::sse::Event, std::convert::Infallible>,
        >(10);
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
        let (tx, _rx) = tokio::sync::mpsc::channel::<
            Result<axum::response::sse::Event, std::convert::Infallible>,
        >(10);
        let mut last_seq = 10_u64;
        let ev = make_prompt_end("SessionPromptEnd", "cancelled", 11);

        let cont = forward_to_client(&tx, &ev, "s", &mut last_seq).await;
        assert!(cont, "cancelled must NOT close the stream");
        assert_eq!(last_seq, 11);
    }

    #[tokio::test]
    async fn forward_to_client_stops_on_error() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<
            Result<axum::response::sse::Event, std::convert::Infallible>,
        >(10);
        let mut last_seq = 10_u64;
        let ev = make_prompt_end("SessionPromptEnd", "error", 0);

        let cont = forward_to_client(&tx, &ev, "s", &mut last_seq).await;
        assert!(!cont, "error is a turn terminal and must close the stream");
    }

    #[tokio::test]
    async fn forward_to_client_stops_on_stream_ended() {
        // rcoder 合成的流替换信号必须关流，否则客户端转发 task hang
        let (tx, _rx) = tokio::sync::mpsc::channel::<
            Result<axum::response::sse::Event, std::convert::Infallible>,
        >(10);
        let mut last_seq = 10_u64;
        let ev = make_prompt_end("SessionPromptEnd", "stream_ended", 0);

        let cont = forward_to_client(&tx, &ev, "s", &mut last_seq).await;
        assert!(
            !cont,
            "stream_ended must close the stream (client should reconnect)"
        );
    }
}
