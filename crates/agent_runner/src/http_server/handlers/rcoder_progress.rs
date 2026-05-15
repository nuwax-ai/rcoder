//! RCoder Agent Progress Handler
//!
//! 处理 GET /agent/progress/{session_id} 请求 (SSE 流)
//!
//! 与 computer_progress.rs 使用相同的 SSE 流模式，直接从 SESSION_CACHE 订阅消息

use std::convert::Infallible;
use std::pin::Pin;
use std::time::Duration;

use axum::{
    Json,
    extract::Path,
    http::{HeaderMap, StatusCode},
    response::sse::{Event, Sse},
};
use chrono::Utc;
use futures_util::stream::{Stream, StreamExt};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, info, warn};

use crate::service::{AGENT_REGISTRY, SESSION_CACHE};
use shared_types::{
    AgentStatus, HttpResult, SessionMessageType, UnifiedSessionMessage,
    error_codes::{ERR_INTERNAL_SERVER_ERROR, ERR_SESSION_NOT_FOUND},
    get_i18n_message,
};

use super::locale_from_headers;

/// 统一的 SSE 流类型
type SseStream = Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

/// 检查 Agent 是否处于 idle 状态
///
/// 返回 true 如果：
/// - Agent 不存在，或
/// - Agent 状态为 Idle，或
/// - Agent 的 session_id 与当前请求的 session_id 不匹配
async fn is_agent_idle(session_id: &str) -> bool {
    // 通过 session_id 获取 agent_info
    if let Some(info) = AGENT_REGISTRY.get_agent_info_by_session(session_id) {
        // 检查状态是否为 Idle，或者 session_id 是否匹配
        info.status == AgentStatus::Idle || info.session_id.to_string() != session_id
    } else {
        // Agent 不存在，视为 idle
        true
    }
}

/// 创建 Agent idle 时的结束事件
///
/// 当 Agent 处于闲置状态时，发送此事件通知前端没有正在执行的任务
fn create_idle_end_event(session_id: &str) -> Event {
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

    let json_data = match serde_json::to_string(&unified_message) {
        Ok(json) => json,
        Err(e) => {
            warn!(
                "⚠️ [RCoder] 序列化 SessionPromptEnd 消息失败: session_id={}, error={}",
                session_id, e
            );
            // 返回包含 session_id 的最小可用结构
            format!(
                r#"{{"sessionId":"{}","messageType":"sessionPromptEnd","subType":"end_turn","data":{{"reason":"EndTurn","description":"Agent 当前无在执行任务"}}}}"#,
                session_id
            )
        }
    };

    Event::default().event("end_turn").data(json_data)
}

/// 创建心跳消息流
///
/// 定期发送符合 UnifiedSessionMessage 格式的心跳消息
fn create_heartbeat_stream(
    session_id: String,
) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    let heartbeat_interval = Duration::from_secs(15);
    let (tx, rx) = tokio::sync::mpsc::channel(10);

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(heartbeat_interval);
        // 立即发送第一个心跳，然后按间隔发送
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            // 创建心跳消息
            let heartbeat_msg = UnifiedSessionMessage::heartbeat(session_id.clone());
            let json_str = match serde_json::to_string(&heartbeat_msg) {
                Ok(s) => s,
                Err(e) => {
                    error!("[RCoder] Failed to serialize heartbeat message: {}", e);
                    continue;
                }
            };

            // 使用 sub_type ("ping") 作为事件名
            if tx
                .send(Ok(Event::default().event("ping").data(json_str)))
                .await
                .is_err()
            {
                // 接收端已关闭，停止发送心跳
                break;
            }
        }
    });

    ReceiverStream::new(rx)
}

/// RCoder Agent 进度流 (SSE)
///
/// 直接从 SESSION_CACHE 订阅消息流，无需 gRPC
#[utoipa::path(
    get,
    path = "/agent/progress/{session_id}",
    params(
        ("session_id" = String, Path, description = "会话ID")
    ),
    responses(
        (status = 200, description = "SSE progress stream", content_type = "text/event-stream"),
        (status = 404, description = "Session not found"),
        (status = 500, description = "Internal server error")
    ),
    tag = "RCoder Agent"
)]
pub async fn handle_rcoder_progress(
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Result<Sse<SseStream>, (StatusCode, Json<HttpResult<String>>)> {
    let locale = locale_from_headers(&headers);
    info!(
        "📡 [RCoder] Progress stream subscribed: session_id={}",
        session_id
    );

    // 0. 检查 Agent 状态（必须在 SESSION_CACHE 查找之前检查）
    if is_agent_idle(&session_id).await {
        info!(
            "💤 [RCoder] Agent is idle, sending SessionPromptEnd and closing: session_id={}",
            session_id
        );
        let end_event = create_idle_end_event(&session_id);
        let stream: SseStream = Box::pin(futures_util::stream::iter([Ok(end_event)]));
        return Ok(Sse::new(stream));
    }

    // 1. 从 SESSION_CACHE 获取 session_data
    // 🛡️ 关键：先 clone Arc<SessionData>，立即释放 DashMap shard 读锁
    let session_data = match SESSION_CACHE.get(&session_id) {
        Some(data) => data.value().clone(),
        None => {
            warn!(" [RCoder] Session not found: session_id={}", session_id);
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

    // 2. 创建新的消息订阅（DashMap 锁已释放，此处 await 安全）
    let (message_rx, _cancel_token) = match session_data.create_new_connection(1000).await {
        Ok(conn) => conn,
        Err(e) => {
            error!(
                "❌ [RCoder] Failed to create session connection: session_id={}, error={}",
                session_id, e
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

    // 3. 创建消息流和心跳流
    let message_stream = ReceiverStream::new(message_rx).map(|msg| {
        let is_terminal = matches!(msg.message_type, SessionMessageType::SessionPromptEnd);
        let json_str = match serde_json::to_string(&msg) {
            Ok(s) => s,
            Err(e) => {
                error!("[RCoder] Failed to serialize message: {}", e);
                return (Ok(Event::default().data("{}")), false);
            }
        };
        (
            Ok(Event::default().event(msg.sub_type).data(json_str)),
            is_terminal,
        )
    });

    // 4. 创建心跳流（标记为非终端）
    let heartbeat_stream = create_heartbeat_stream(session_id.clone()).map(|event| (event, false));

    // 5. 合并两个流，并用 scan 监测终止条件
    // select 会继续轮询心跳流（永不结束），所以必须在合并流层面检测终止
    // 终止条件：
    //   - 收到 SessionPromptEnd（终端消息）→ 发送后结束流
    //   - channel 关闭 → message_stream 返回 None，scan 最终也会结束
    let merged_stream = futures_util::stream::select(message_stream, heartbeat_stream).scan(
        false,
        |seen_terminal, (event, is_terminal)| {
            if *seen_terminal {
                // 已发送终端消息，结束流
                return std::future::ready(None);
            }

            if is_terminal {
                *seen_terminal = true;
            }

            std::future::ready(Some(event))
        },
    );

    info!("[RCoder] SSE stream established: session_id={}", session_id);

    let stream: SseStream = Box::pin(merged_stream);
    Ok(Sse::new(stream))
}
