//! SubscribeProgress RPC 实现

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

use super::locale::{locale_from_grpc_request, localized};

pub type SubscribeProgressStream = Pin<Box<dyn Stream<Item = Result<ProgressEvent, Status>> + Send>>;

/// 空闲超时时间（5分钟）
/// 如果在这个时间内没有收到任何真实消息（不包括心跳），则关闭流
const IDLE_TIMEOUT_SECS: u64 = 300;

#[instrument(skip(_app_state, request))]
pub async fn subscribe_progress(
    _app_state: &Arc<AppState>,
    request: Request<ProgressRequest>,
) -> Result<Response<SubscribeProgressStream>, Status> {
    let locale = locale_from_grpc_request(&request);
    shared_types::scope_request_locale(locale, async move {
        let req = request.into_inner();
        let session_id = req.session_id.clone();

        info!(
            "📡 [gRPC] SubscribeProgress started: session_id={}",
            session_id
        );

        let (tx, rx) = mpsc::channel::<Result<ProgressEvent, Status>>(100);
        let session_id_clone = session_id.clone();

        tokio::spawn(async move {
            use dashmap::mapref::entry::Entry;

            // 🛡️ 关键修复：不在 DashMap entry() 持锁范围内调用 .await
            // view() 在闭包返回后立即释放锁，无 Ref 暴露
            let session_data = if let Some(existing) = SESSION_CACHE.view(&session_id_clone, |_, d| d.clone()) {
                info!(
                    "📦 [gRPC] SESSION_CACHE already exists, reusing: session_id={}",
                    session_id_clone
                );
                existing
            } else {
                let session_data = crate::service::SessionData::new(1000).await;
                match SESSION_CACHE.entry(session_id_clone.clone()) {
                    Entry::Occupied(entry) => {
                        info!(
                            "📦 [gRPC] SESSION_CACHE already exists, reusing: session_id={}",
                            session_id_clone
                        );
                        entry.get().clone()
                    }
                    Entry::Vacant(entry) => {
                        info!(
                            "🆕 [gRPC] SESSION_CACHE does not exist, creating new: session_id={}",
                            session_id_clone
                        );
                        entry.insert(session_data.clone());
                        session_data
                    }
                }
            };

            match session_data.create_new_connection(100).await {
                Ok((mut message_rx, cancellation_token)) => {
                    info!("📡 [gRPC] Session connection created successfully: {}", session_id_clone);

                    let mut last_message_time = Instant::now();
                    #[allow(unused_assignments)]
                    let mut exit_reason = "unknown";

                    loop {
                        tokio::select! {
                            _ = cancellation_token.cancelled() => {
                                info!("📡 [gRPC] Session connection cancelled, sending SessionPromptEnd: session_id={}", session_id_clone);
                                exit_reason = "cancelled";

                                use agent_client_protocol::schema::StopReason;
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

                                if let Err(e) = tx.send(Ok(end_event)).await {
                                    warn!("📡 [gRPC] Failed to send SessionPromptEnd event: session_id={}, error={}", session_id_clone, e);
                                }

                                break;
                            }
                            msg = message_rx.recv() => {
                                match msg {
                                    Some(unified_message) => {
                                        // 重置空闲超时计时器
                                        last_message_time = Instant::now();

                                        let is_terminal_message = matches!(
                                            unified_message.message_type,
                                            crate::model::SessionMessageType::SessionPromptEnd
                                        );

                                        let event = unified_message_to_progress_event(&unified_message);
                                        if tx.send(Ok(event)).await.is_err() {
                                            debug!("📡 [gRPC] Client disconnected");
                                            exit_reason = "client_disconnected";
                                            break;
                                        }

                                        if is_terminal_message {
                                            info!(
                                                "🔚 [gRPC] Received SessionPromptEnd, closing stream: session_id={}, sub_type={}",
                                                session_id_clone, unified_message.sub_type
                                            );
                                            exit_reason = "terminal_message";
                                            break;
                                        }
                                    }
                                    None => {
                                        debug!("📡 [gRPC] Session channel closed, sending SessionPromptEnd event");
                                        exit_reason = "channel_closed";
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
                            _ = tokio::time::sleep(Duration::from_secs(30)) => {
                                // 检查空闲超时
                                if last_message_time.elapsed() > Duration::from_secs(IDLE_TIMEOUT_SECS) {
                                    info!(
                                        "⏱️ [gRPC] Idle timeout ({}s) reached, closing stream: session_id={}",
                                        IDLE_TIMEOUT_SECS, session_id_clone
                                    );
                                    let timeout_event = ProgressEvent {
                                        message_type: "SessionPromptEnd".to_string(),
                                        sub_type: "idle_timeout".to_string(),
                                        payload: format!(
                                            r#"{{"reason":"IdleTimeout","description":"{}"}}"#,
                                            localized(
                                                locale,
                                                "会话空闲超时",
                                                "會話空閒超時",
                                                "Session idle timeout",
                                            )
                                        ),
                                        request_id: None,
                                        timestamp: chrono::Utc::now().timestamp_millis(),
                                    };
                                    let _ = tx.send(Ok(timeout_event)).await;
                                    exit_reason = "idle_timeout";
                                    break;
                                }

                                let heartbeat = ProgressEvent {
                                    message_type: "Heartbeat".to_string(),
                                    sub_type: "ping".to_string(),
                                    payload: r#"{"type":"heartbeat","message":"keep-alive"}"#.to_string(),
                                    request_id: None,
                                    timestamp: chrono::Utc::now().timestamp_millis(),
                                };

                                if tx.send(Ok(heartbeat)).await.is_err() {
                                    debug!("📡 [gRPC] Failed to send heartbeat; client disconnected");
                                    exit_reason = "heartbeat_failed";
                                    break;
                                }
                            }
                        }
                    }

                    // gRPC 流结束时清理 SSE sender
                    // 防止下次 Chat 请求时 try_send 使用已关闭的旧 sender 导致消息丢失
                    info!("🔌 [gRPC] SubscribeProgress stream ended, cleaning up SSE sender: session_id={}, reason={}", session_id_clone, exit_reason);
                    session_data.close_current_connection().await;
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
                            "📡 [gRPC] Failed to send error status: session_id={}, error={}",
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

fn unified_message_to_progress_event(
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
        timestamp,
    }
}
