//! SubscribeProgress RPC 实现

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use shared_types::grpc::{ProgressEvent, ProgressRequest};
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{debug, info, instrument, warn};

use crate::router::AppState;
use crate::service::SESSION_CACHE;

use super::locale::locale_from_grpc_request;

pub type SubscribeProgressStream =
    Pin<Box<dyn Stream<Item = Result<ProgressEvent, Status>> + Send>>;

/// 空闲超时时间（30分钟）
/// 如果在这个时间内没有收到任何真实消息（不包括心跳），则关闭流
const IDLE_TIMEOUT_SECS: u64 = 1800;

/// 流退出原因，每个变体对应一个明确的退出路径
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitReason {
    /// 用户主动取消
    Cancelled,
    /// 客户端断开连接（tx.send 失败）
    ClientDisconnected,
    /// 收到 SessionPromptEnd 终止消息
    TerminalMessage,
    /// 消息通道关闭（receiver 返回 None）
    ChannelClosed,
    /// 空闲超时
    IdleTimeout,
    /// 心跳发送失败（客户端断开）
    HeartbeatFailed,
}

impl fmt::Display for ExitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => write!(f, "cancelled"),
            Self::ClientDisconnected => write!(f, "client_disconnected"),
            Self::TerminalMessage => write!(f, "terminal_message"),
            Self::ChannelClosed => write!(f, "channel_closed"),
            Self::IdleTimeout => write!(f, "idle_timeout"),
            Self::HeartbeatFailed => write!(f, "heartbeat_failed"),
        }
    }
}

#[instrument(skip(_app_state, request))]
pub async fn subscribe_progress(
    _app_state: &Arc<AppState>,
    request: Request<ProgressRequest>,
) -> Result<Response<SubscribeProgressStream>, Status> {
    let locale = locale_from_grpc_request(&request);
    shared_types::scope_request_locale(locale, async move {
        let req = request.into_inner();
        let session_id = req.session_id.clone();
        // 订阅方的消费游标：只回放 seq > from_seq 的消息（增量补齐）。
        // 缺省（None / 0）= 全量回放（向后兼容）。
        let from_seq = req.from_seq.unwrap_or(0);

        info!(
            "[gRPC] SubscribeProgress started: session_id={}",
            session_id
        );

        let (tx, rx) = mpsc::channel::<Result<ProgressEvent, Status>>(100);
        let session_id_clone = session_id.clone();

        tokio::spawn(async move {
            use dashmap::mapref::entry::Entry;

            info!(
                "[gRPC] Looking up session in SESSION_CACHE: session_id={}",
                session_id_clone
            );
            // 🛡️ 关键修复：不在 DashMap entry() 持锁范围内调用 .await
            // view() 在闭包返回后立即释放锁，无 Ref 暴露
            let session_data = if let Some(existing) = SESSION_CACHE.view(&session_id_clone, |_, d| d.clone()) {
                info!(
                    "[gRPC] SESSION_CACHE found, reusing: session_id={}",
                    session_id_clone
                );
                existing
            } else {
                info!(
                    "[gRPC] SESSION_CACHE not found, creating new SessionData: session_id={}",
                    session_id_clone
                );
                let session_data = crate::service::SessionData::new(1000).await;
                match SESSION_CACHE.entry(session_id_clone.clone()) {
                    Entry::Occupied(entry) => {
                        info!(
                            "[gRPC] SESSION_CACHE exists after creation, reusing: session_id={}",
                            session_id_clone
                        );
                        entry.get().clone()
                    }
                    Entry::Vacant(entry) => {
                        info!(
                            "[gRPC] SESSION_CACHE created successfully: session_id={}",
                            session_id_clone
                        );
                        entry.insert(session_data.clone());
                        session_data
                    }
                }
            };

            info!(
                "[gRPC] Creating new SSE connection: session_id={}",
                session_id_clone
            );
            match session_data.create_new_connection(100, from_seq).await {
                Ok((replay_messages, message_rx, cancellation_token)) => {
                    info!(
                        "[gRPC] Session connection created successfully: session_id={}, replay_count={}",
                        session_id_clone, replay_messages.len()
                    );

                    // 📼 回放 ring buffer 中的历史消息
                    for (seq, msg) in replay_messages {
                        let event = unified_message_to_progress_event(seq, &msg);
                        debug!(
                            "[gRPC] Replaying ProgressEvent: session_id={}, message_type={}, sub_type={}, payload={}",
                            session_id_clone, event.message_type, event.sub_type, event.payload
                        );
                        if tx.send(Ok(event)).await.is_err() {
                            debug!("[gRPC] Client disconnected during replay");
                            session_data.close_current_connection().await;
                            return;
                        }
                    }

                    let reason = run_stream_loop(
                        &session_id_clone,
                        message_rx,
                        cancellation_token,
                        &tx,
                        locale,
                    )
                    .await;

                    info!(
                        "[gRPC] SubscribeProgress stream ended, cleaning up SSE sender: session_id={}, reason={}",
                        session_id_clone, reason
                    );
                    session_data.close_current_connection().await;
                }
                Err(e) => {
                    warn!("[gRPC] Failed to create session connection: {}", e);
                    if let Err(send_err) = tx
                        .send(Err(Status::internal(format!(
                            "{}: {}",
                            shared_types_i18n::get_i18n_message("grpc.subscribe.create_connection_failed", locale),
                            e
                        ))))
                        .await
                    {
                        warn!(
                            "[gRPC] Failed to send error status: session_id={}, error={}",
                            session_id_clone, send_err
                        );
                    }
                }
            }
        });

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(
            Box::pin(stream) as SubscribeProgressStream
        ))
    })
    .await
}

/// 运行 SSE 流主循环，返回退出原因
///
/// 每个分支通过 `return` 显式返回 `ExitReason`，编译器可追踪所有路径。
/// 独立函数避免了 `tokio::select!` 宏展开导致的 definite-assignment 盲区。
async fn run_stream_loop(
    session_id: &str,
    mut message_rx: tokio::sync::mpsc::Receiver<(u64, shared_types::UnifiedSessionMessage)>,
    cancellation_token: tokio_util::sync::CancellationToken,
    tx: &mpsc::Sender<Result<ProgressEvent, Status>>,
    locale: &'static str,
) -> ExitReason {
    let mut last_message_time = Instant::now();

    loop {
        tokio::select! {
            _ = cancellation_token.cancelled() => {
                info!("[gRPC] Session connection cancelled, sending SessionPromptEnd: session_id={}", session_id);

                use agent_client_protocol::schema::v1::StopReason;
                use shared_types::{SessionNotify, SessionPromptEnd};

                let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
                    session_id: session_id.to_string(),
                    stop_reason: StopReason::Cancelled,
                    error_message: Some(shared_types_i18n::get_i18n_message("grpc.subscribe.user_cancelled", locale)),
                    request_id: None,
                });
                let unified_message = notify.to_unified_message();
                let end_event = unified_message_to_progress_event(0, &unified_message);

                if let Err(e) = tx.send(Ok(end_event)).await {
                    warn!("[gRPC] Failed to send SessionPromptEnd event: session_id={}, error={}", session_id, e);
                }

                return ExitReason::Cancelled;
            }
            msg = message_rx.recv() => {
                match msg {
                    Some((seq, unified_message)) => {
                        // 重置空闲超时计时器
                        last_message_time = Instant::now();

                        let is_terminal_message = matches!(
                            unified_message.message_type,
                            crate::model::SessionMessageType::SessionPromptEnd
                        );

                        let event = unified_message_to_progress_event(seq, &unified_message);
                        debug!(
                            "[gRPC] Sending ProgressEvent: session_id={}, message_type={}, sub_type={}, payload={}",
                            session_id, event.message_type, event.sub_type, event.payload
                        );
                        if tx.send(Ok(event)).await.is_err() {
                            debug!("[gRPC] Client disconnected");
                            return ExitReason::ClientDisconnected;
                        }

                        if is_terminal_message {
                            info!(
                                "[gRPC] Received SessionPromptEnd, closing stream: session_id={}, sub_type={}",
                                session_id, unified_message.sub_type
                            );
                            return ExitReason::TerminalMessage;
                        }
                    }
                    None => {
                        debug!("[gRPC] Session channel closed, sending SessionPromptEnd event");
                        let end_event = ProgressEvent {
                            message_type: "SessionPromptEnd".to_string(),
                            sub_type: "end_turn".to_string(),
                            payload: format!(
                                r#"{{"reason":"EndTurn","description":"{}"}}"#,
                                shared_types_i18n::get_i18n_message("grpc.subscribe.no_active_task", locale)
                            ),
                            request_id: None,
                            seq: 0,
                            timestamp: chrono::Utc::now().timestamp_millis(),
                        };
                        if let Err(e) = tx.send(Ok(end_event)).await {
                            warn!("[gRPC] Failed to send SessionPromptEnd event: session_id={}, error={}", session_id, e);
                        }
                        return ExitReason::ChannelClosed;
                    }
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(30)) => {
                // 检查空闲超时
                if last_message_time.elapsed() > Duration::from_secs(IDLE_TIMEOUT_SECS) {
                    info!(
                        "[gRPC] Idle timeout ({}s) reached, closing stream: session_id={}",
                        IDLE_TIMEOUT_SECS, session_id
                    );
                    let timeout_event = ProgressEvent {
                        message_type: "SessionPromptEnd".to_string(),
                        sub_type: "idle_timeout".to_string(),
                        payload: format!(
                            r#"{{"reason":"IdleTimeout","description":"{}"}}"#,
                            shared_types_i18n::get_i18n_message("grpc.subscribe.idle_timeout", locale)
                        ),
                        request_id: None,
                        seq: 0,
                        timestamp: chrono::Utc::now().timestamp_millis(),
                    };
                    let _ = tx.send(Ok(timeout_event)).await;
                    return ExitReason::IdleTimeout;
                }

                let heartbeat = ProgressEvent {
                    message_type: "Heartbeat".to_string(),
                    sub_type: "ping".to_string(),
                    payload: r#"{"type":"heartbeat","message":"keep-alive"}"#.to_string(),
                    request_id: None,
                    seq: 0,
                    timestamp: chrono::Utc::now().timestamp_millis(),
                };

                if tx.send(Ok(heartbeat)).await.is_err() {
                    debug!("[gRPC] Failed to send heartbeat; client disconnected");
                    return ExitReason::HeartbeatFailed;
                }
            }
        }
    }
}

fn unified_message_to_progress_event(
    seq: u64,
    message: &shared_types::UnifiedSessionMessage,
) -> ProgressEvent {
    let timestamp = message.timestamp.timestamp_millis();

    ProgressEvent {
        message_type: format!("{:?}", message.message_type),
        sub_type: message.sub_type.clone(),
        payload: serde_json::to_string(&message.data).unwrap_or_default(),
        request_id: message
            .data
            .get("request_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        seq,
        timestamp,
    }
}
