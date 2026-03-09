//! Computer Agent Progress Handler
//!
//! 处理 GET /computer/progress/{session_id} 请求 (SSE 流)

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
};
use chrono::Utc;
use futures_util::stream::{Stream, StreamExt};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, info, warn};

use crate::http_server::router::AppState;
use crate::service::{AGENT_REGISTRY, SESSION_CACHE};
use shared_types::{AgentStatus, HttpResult, SessionMessageType, UnifiedSessionMessage};

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
                "⚠️ [HTTP] 序列化 SessionPromptEnd 消息失败: session_id={}, error={}",
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
                    error!("❌ [HTTP] 心跳消息序列化失败: {}", e);
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

/// Computer Agent 进度流 (SSE)
///
/// 直接从 SESSION_CACHE 订阅消息流,无需 gRPC
#[utoipa::path(
    get,
    path = "/computer/progress/{session_id}",
    responses(
        (status = 200, description = "SSE progress stream", content_type = "text/event-stream"),
        (status = 404, description = "Session not found"),
        (status = 500, description = "Internal server error")
    ),
    tag = "Computer Agent"
)]
pub async fn handle_computer_progress(
    State(_state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<Sse<SseStream>, (StatusCode, Json<HttpResult<String>>)> {
    info!(
        "📡 [HTTP] Computer Agent 进度流订阅: session_id={}",
        session_id
    );

    // 0. 检查 Agent 状态（必须在 SESSION_CACHE 查找之前检查）
    if is_agent_idle(&session_id).await {
        info!(
            "💤 [HTTP] Agent 处于 idle 状态，发送 SessionPromptEnd 并关闭: session_id={}",
            session_id
        );
        let end_event = create_idle_end_event(&session_id);
        let stream: SseStream = Box::pin(futures_util::stream::iter([Ok(end_event)]));
        return Ok(Sse::new(stream));
    }

    // 1. 从 SESSION_CACHE 获取 session_data
    // 🛡️ 关键修复：先 clone Arc<SessionData>，立即释放 DashMap shard 读锁
    // 之前直接在 Ref 上调用 create_new_connection().await，导致 DashMap 读锁跨 await 持有
    // 可能造成与 SESSION_CACHE.entry()/remove() 等写操作的死锁
    let session_data = match SESSION_CACHE.get(&session_id) {
        Some(data) => data.value().clone(),
        None => {
            warn!("⚠️  [HTTP] Session 不存在: session_id={}", session_id);
            return Err((
                StatusCode::NOT_FOUND,
                Json(HttpResult::error(
                    "SESSION_NOT_FOUND",
                    &format!("Session {} not found", session_id),
                )),
            ));
        }
    };

    // 2. 创建新的消息订阅（DashMap 锁已释放，此处 await 安全）
    let (message_rx, _cancel_token) = match session_data.create_new_connection(1000).await {
        Ok(conn) => conn,
        Err(e) => {
            error!(
                "❌ [HTTP] 创建 session 连接失败: session_id={}, error={}",
                session_id, e
            );
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(HttpResult::error(
                    "INTERNAL_ERROR",
                    &format!("Failed to create session connection: {}", e),
                )),
            ));
        }
    };

    // 3. 创建消息流和心跳流
    // UnifiedSessionMessage 已使用 #[serde(rename_all = "camelCase")], 序列化后符合 RCoder 约定
    let message_stream = ReceiverStream::new(message_rx).map(|msg| {
        // 直接序列化 UnifiedSessionMessage（使用 camelCase）
        let json_str = match serde_json::to_string(&msg) {
            Ok(s) => s,
            Err(e) => {
                error!("❌ [HTTP] 消息序列化失败: {}", e);
                return Ok(Event::default().data("{}"));
            }
        };

        // 使用 sub_type 作为 SSE event name（与 RCoder 约定一致）
        Ok(Event::default().event(msg.sub_type).data(json_str))
    });

    // 4. 创建心跳流
    let heartbeat_stream = create_heartbeat_stream(session_id.clone());

    // 5. 合并两个流（消息流优先级更高，心跳作为背景流）
    // 使用 select! 合并，保持事件顺序
    let merged_stream = futures_util::stream::select(message_stream, heartbeat_stream);

    info!("✅ [HTTP] SSE 流已建立: session_id={}", session_id);

    let stream: SseStream = Box::pin(merged_stream);
    Ok(Sse::new(stream))
}
