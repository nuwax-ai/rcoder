//! 进度 SSE 流的共享实现
//!
//! `/computer/progress/{session_id}`、`/agent/progress/{session_id}`、
//! `/devcomputer/progress/{session_id}` 三条路由的流式逻辑完全同源：都从同一批全局
//! 单例 `AGENT_REGISTRY` / `SESSION_CACHE` 读取，事件类型（`UnifiedSessionMessage` /
//! `SessionPromptEnd` / `ping` / `end_turn`）与 session 查找逻辑逐字节相同。
//!
//! 三者唯一实质差异是**日志前缀**（运维按前缀 grep，不得静默改变），故以
//! `log_prefix: &'static str` 参数化；路由路径与 utoipa 元数据留在各薄 handler 上。
//!
//! 此前 `computer_progress.rs` 与 `rcoder_progress.rs` 各持一份 341 行副本
//! （310 行相同），任何修复都得改两遍——本模块是那次收敛的结果。

use std::convert::Infallible;
use std::pin::Pin;
use std::time::Duration;

use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::sse::{Event, Sse},
};
use chrono::Utc;
use futures_util::stream::Stream;
use tracing::{error, info, warn};

use crate::service::{AGENT_REGISTRY, SESSION_CACHE};
use shared_types::{
    AgentStatus, HttpResult, SessionMessageType, UnifiedSessionMessage,
    error_codes::{ERR_INTERNAL_SERVER_ERROR, ERR_SESSION_NOT_FOUND},
    get_i18n_message,
};

use super::locale_from_headers;

/// 统一的 SSE 流类型
///
/// `pub(crate)`：此前各 handler 模块私有，导致 `devcomputer_handlers` 无法直接引用
/// 返回类型、只能包一层 `impl IntoResponse` 适配。
pub(crate) type SseStream = Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

/// 进度流的返回类型（三条路由共用）
pub(crate) type ProgressResponse = Result<Sse<SseStream>, (StatusCode, Json<HttpResult<String>>)>;

/// 检查 Agent 是否处于 idle 状态
///
/// 返回 Some(true) 如果 Agent 确实处于 Idle 状态
/// 返回 Some(false) 如果 Agent 处于活跃状态
/// 返回 None 如果 Agent 不存在（可能是竞态条件，session 还未注册）
async fn check_agent_idle(session_id: &str, log_prefix: &'static str) -> Option<bool> {
    // 通过 session_id 获取 agent_info
    if let Some(info) = AGENT_REGISTRY.get_agent_info_by_session(session_id) {
        // 检查状态是否为 Idle，或者 session_id 是否匹配
        Some(info.status == AgentStatus::Idle || info.session_id.to_string() != session_id)
    } else {
        // Agent 不存在，返回 None（可能是竞态条件）
        warn!(
            "⚠️ [{}] Agent not found in registry for session_id={}, possible race condition",
            log_prefix, session_id
        );
        None
    }
}

/// 创建 Agent idle 时的结束事件
///
/// 当 Agent 处于闲置状态时，发送此事件通知前端没有正在执行的任务
fn create_idle_end_event(session_id: &str, log_prefix: &'static str) -> Event {
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

    let json_data = serde_json::to_string(&unified_message).unwrap_or_else(|e| {
        warn!(
            "[{}] Failed to serialize SessionPromptEnd message: session_id={}, error={}",
            log_prefix, session_id, e
        );
        // 降级处理：返回最简 JSON
        format!(
            r#"{{"sessionId":"{}","messageType":"sessionPromptEnd","subType":"end_turn"}}"#,
            session_id
        )
    });

    Event::default().event("end_turn").data(json_data)
}

/// 创建 Agent 不存在时的错误事件
///
/// 当 Agent 在 AGENT_REGISTRY 中不存在时，发送此事件通知前端
/// 包含错误信息和 end_turn 消息，方便排查定位问题
fn create_agent_not_found_event(session_id: &str, log_prefix: &'static str) -> Event {
    let unified_message = UnifiedSessionMessage {
        session_id: session_id.to_string(),
        message_type: SessionMessageType::SessionPromptEnd,
        sub_type: "end_turn".to_string(),
        data: serde_json::json!({
            "reason": "AgentNotFound",
            "description": format!("Agent not found in registry for session: {}", session_id)
        }),
        timestamp: Utc::now(),
    };

    let json_data = serde_json::to_string(&unified_message).unwrap_or_else(|e| {
        warn!(
            "[{}] Failed to serialize AgentNotFound message: session_id={}, error={}",
            log_prefix, session_id, e
        );
        // 降级处理：返回最简 JSON
        format!(
            r#"{{"sessionId":"{}","messageType":"sessionPromptEnd","subType":"end_turn"}}"#,
            session_id
        )
    });

    Event::default().event("end_turn").data(json_data)
}

/// 进度流 (SSE) 的共享实现
///
/// 直接从 SESSION_CACHE 订阅消息流，无需 gRPC。
/// `log_prefix` 只影响日志文本，不影响任何 wire 格式。
pub(crate) async fn progress_sse(
    headers: HeaderMap,
    session_id: String,
    log_prefix: &'static str,
) -> ProgressResponse {
    let locale = locale_from_headers(&headers);

    // 0. 检查 Agent 状态（必须在 SESSION_CACHE 查找之前检查）
    // 只有当 Agent 确实存在且处于 Idle 状态时，才发送 SessionPromptEnd
    // 如果 Agent 不存在，发送错误信息 + end_turn 消息，然后关闭 SSE 连接
    match check_agent_idle(&session_id, log_prefix).await {
        Some(true) => {
            info!(
                "💤 [{}] Agent is idle (confirmed), sending SessionPromptEnd and closing: session_id={}",
                log_prefix, session_id
            );
            let end_event = create_idle_end_event(&session_id, log_prefix);
            let stream: SseStream = Box::pin(futures_util::stream::iter([Ok(end_event)]));
            return Ok(Sse::new(stream));
        }
        Some(false) => {
            info!(
                "🔄 [{}] Agent is active, continuing to establish stream: session_id={}",
                log_prefix, session_id
            );
        }
        None => {
            // Agent 不存在，发送错误信息 + end_turn 消息，然后关闭 SSE 连接
            warn!(
                "⚠️ [{}] Agent not found in registry, sending error and end_turn: session_id={}",
                log_prefix, session_id
            );
            let error_event = create_agent_not_found_event(&session_id, log_prefix);
            let stream: SseStream = Box::pin(futures_util::stream::iter([Ok(error_event)]));
            return Ok(Sse::new(stream));
        }
    }

    // 1. 从 SESSION_CACHE 获取 session_data
    // 🛡️ 关键修复：先 clone Arc<SessionData>，立即释放 DashMap shard 读锁
    // 之前直接在 Ref 上调用 create_new_connection().await，导致 DashMap 读锁跨 await 持有
    // 可能造成与 SESSION_CACHE.entry()/remove() 等写操作的死锁
    // view() 在闭包返回后立即释放锁，无 Ref 暴露
    info!(
        "[{}] Looking up session in SESSION_CACHE: session_id={}",
        log_prefix, session_id
    );
    let session_data = match SESSION_CACHE.view(&session_id, |_, d| d.clone()) {
        Some(data) => {
            info!(
                "[{}] SESSION_CACHE found for session_id={}",
                log_prefix, session_id
            );
            data
        }
        None => {
            warn!(
                "[{}] Session not found in SESSION_CACHE: session_id={}",
                log_prefix, session_id
            );
            return Err((
                StatusCode::NOT_FOUND,
                Json(HttpResult::error_with_message(
                    ERR_SESSION_NOT_FOUND,
                    locale,
                    &format!(
                        "{}: {}",
                        get_i18n_message("error.session_not_found", locale),
                        session_id
                    ),
                )),
            ));
        }
    };

    info!(
        "[{}] Creating new SSE connection: session_id={}",
        log_prefix, session_id
    );
    // 2. 创建新的消息订阅（DashMap 锁已释放，此处 await 安全）
    let (conn_id, replay_messages, message_rx, cancel_token) =
        match session_data.create_new_connection(1000, 0).await {
            Ok(conn) => conn,
            Err(e) => {
                error!(
                    "❌ [{}] Failed to create session connection: session_id={}, error={}",
                    log_prefix, session_id, e
                );
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(HttpResult::error_with_message(
                        ERR_INTERNAL_SERVER_ERROR,
                        locale,
                        &format!(
                            "{}: {}",
                            get_i18n_message("error.internal_server_error", locale),
                            e
                        ),
                    )),
                ));
            }
        };

    let stream = subscription_stream(
        session_data,
        conn_id,
        session_id,
        log_prefix,
        replay_messages,
        message_rx,
        cancel_token,
    );
    Ok(Sse::new(stream))
}

struct SubscriptionGuard {
    session: std::sync::Arc<crate::service::SessionData>,
    conn_id: u64,
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        self.session.close_connection(self.conn_id);
    }
}

fn subscription_stream(
    session: std::sync::Arc<crate::service::SessionData>,
    conn_id: u64,
    session_id: String,
    log_prefix: &'static str,
    replay_messages: Vec<(u64, UnifiedSessionMessage)>,
    mut message_rx: tokio::sync::mpsc::Receiver<(u64, UnifiedSessionMessage)>,
    cancel: tokio_util::sync::CancellationToken,
) -> SseStream {
    // Capture the guard before first poll, so dropping an unpolled response also cleans up.
    let registration = SubscriptionGuard { session, conn_id };
    info!(
        "[{}] SSE stream established: session_id={}",
        log_prefix, session_id
    );
    Box::pin(async_stream::stream! {
        let _registration = registration;
        let mut replay = replay_messages.into_iter();
        let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if cancel.is_cancelled() { break; }
            let message = if let Some((_, message)) = replay.next() {
                message
            } else {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    message = message_rx.recv() => {
                        match message { Some((_, message)) => message, None => break }
                    }
                    _ = heartbeat.tick() => UnifiedSessionMessage::heartbeat(session_id.clone()),
                }
            };
            let terminal = matches!(message.message_type, SessionMessageType::SessionPromptEnd);
            match serde_json::to_string(&message) {
                Ok(json) => yield Ok(Event::default().event(message.sub_type).data(json)),
                Err(error) => {
                    error!(%error, log_prefix, "Failed to serialize session message");
                    break;
                }
            }
            if terminal { break; }
        }
    })
}

#[cfg(test)]
mod subscription_tests {
    use super::*;
    use futures_util::StreamExt;
    #[tokio::test]
    async fn cancelled_subscription_ends_and_releases_its_registration() {
        let session = crate::service::SessionData::new(64).await;
        let (id, replay, rx, cancel) = session.create_new_connection(8, 0).await.unwrap();
        let mut stream = subscription_stream(
            session.clone(),
            id,
            "test".into(),
            "test",
            replay,
            rx,
            cancel.clone(),
        );
        cancel.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), stream.next())
                .await
                .unwrap()
                .is_none()
        );
        drop(stream);
        assert_eq!(session.connections_len(), 0);
    }
    #[tokio::test]
    async fn dropping_unpolled_stream_only_removes_its_subscription() {
        let session = crate::service::SessionData::new(64).await;
        let (id, replay, rx, cancel) = session.create_new_connection(8, 0).await.unwrap();
        let (_peer, _, _peer_rx, peer_cancel) = session.create_new_connection(8, 0).await.unwrap();
        let stream = subscription_stream(
            session.clone(),
            id,
            "test".into(),
            "test",
            replay,
            rx,
            cancel.clone(),
        );
        drop(stream);
        assert!(cancel.is_cancelled());
        assert!(!peer_cancel.is_cancelled());
        assert_eq!(session.connections_len(), 1);
    }

    #[tokio::test]
    async fn closed_message_channel_ends_without_heartbeats() {
        let session = crate::service::SessionData::new(64).await;
        let (id, replay, _original_rx, _) = session.create_new_connection(8, 0).await.unwrap();
        let (sender, rx) = tokio::sync::mpsc::channel(1);
        drop(sender);
        let mut stream = subscription_stream(
            session.clone(),
            id,
            "test".into(),
            "test",
            replay,
            rx,
            tokio_util::sync::CancellationToken::new(),
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), stream.next())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(session.connections_len(), 0);
    }

    #[tokio::test]
    async fn replay_then_live_terminal_is_ordered_and_closes_immediately() {
        use axum::response::IntoResponse;
        let session = crate::service::SessionData::new(64).await;
        let mut first = UnifiedSessionMessage::heartbeat("test".into());
        first.message_type = SessionMessageType::AgentSessionUpdate;
        first.sub_type = "first".into();
        session.push_message(first).await.unwrap();
        let (id, replay, rx, cancel) = session.create_new_connection(8, 0).await.unwrap();
        let mut terminal = UnifiedSessionMessage::heartbeat("test".into());
        terminal.message_type = SessionMessageType::SessionPromptEnd;
        terminal.sub_type = "end_turn".into();
        session.push_message(terminal).await.unwrap();
        let stream = subscription_stream(
            session.clone(),
            id,
            "test".into(),
            "test",
            replay,
            rx,
            cancel,
        );
        let body = Sse::new(stream).into_response().into_body();
        let data = tokio::time::timeout(Duration::from_secs(1), axum::body::to_bytes(body, 65536))
            .await
            .unwrap()
            .unwrap();
        let text = std::str::from_utf8(&data).unwrap();
        assert!(text.find("event: first").unwrap() < text.find("event: end_turn").unwrap());
        assert_eq!(text.matches("event: end_turn").count(), 1);
        assert_eq!(session.connections_len(), 0);
    }
}
