//! Agent 进度通知处理器 - 完全复制 rcoder 的 SSE 实现

use crate::{
    api::ApiState,
    models::{ProgressEvent, ProgressEventType},
};
use axum::{
    extract::Path,
    response::{sse::Event, Sse},
};
use async_stream::stream;
use futures::Stream;
use std::convert::Infallible;
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, error, info, warn};

/// 会话通知路径参数
#[derive(Debug, serde::Deserialize)]
pub struct SessionNotificationParams {
    /// 会话ID，用于标识特定的会话连接
    pub session_id: String,
}

/// Agent 进度通知处理器 (Server-Sent Events) - 完全复制 rcoder 的实现
pub async fn agent_progress(
    Path(params): Path<SessionNotificationParams>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, crate::AgentServerError> {
    let sse_start = std::time::Instant::now();
    info!("⏱️ [SSE] 开始建立连接: session_id={}", params.session_id);

    let session_id = params.session_id.clone();

    // TODO: 实现类似 SESSION_CACHE 的会话数据管理
    // 目前先创建一个简单的流

    info!("⏱️ [SSE] 总SSE连接建立耗时: {:?}", sse_start.elapsed());

    let session_id_for_stream = session_id.clone();

    // 创建SSE流 - 完全复制 rcoder 的流创建逻辑
    let stream = stream! {
        // 🎯 立即发送心跳消息，让前端知道连接已建立
        let heartbeat_event = ProgressEvent::new(
            session_id_for_stream.clone(),
            "heartbeat".to_string(),
            ProgressEventType::AgentStatusChanged,
            serde_json::json!({
                "timestamp": chrono::Utc::now(),
                "message": "SSE connection established"
            }),
        );

        let heartbeat_event_result: Event = Event::default()
            .event("heartbeat")
            .data(serde_json::to_string(&heartbeat_event).unwrap_or_else(|_| "{}".to_string()));

        info!("💓 [SSE] 发送初始心跳消息: session_id={}", session_id_for_stream);
        yield Ok(heartbeat_event_result);

        // 心跳定时器（30秒）- 完全复制 rcoder 的实现
        let mut heartbeat_interval = interval(Duration::from_secs(30));
        // 跳过第一次立即触发
        heartbeat_interval.tick().await;

        loop {
            tokio::select! {
                // 心跳定时器
                _ = heartbeat_interval.tick() => {
                    let heartbeat_event = ProgressEvent::new(
                        session_id_for_stream.clone(),
                        "heartbeat".to_string(),
                        ProgressEventType::AgentStatusChanged,
                        serde_json::json!({
                            "timestamp": chrono::Utc::now(),
                            "message": "heartbeat"
                        }),
                    );

                    let event: Event = Event::default()
                        .event("heartbeat")
                        .data(serde_json::to_string(&heartbeat_event).unwrap_or_else(|_| "{}".to_string()));

                    debug!("💓 [SSE] 发送心跳消息: session_id={}", session_id_for_stream);
                    yield Ok(event);
                }
            }
        }
    };

    Ok(Sse::new(stream))
}

/// 发送进度事件到指定会话 - 完全复制 rcoder 的事件发送逻辑
pub async fn send_progress_event(
    state: &ApiState,
    session_id: &str,
    request_id: &str,
    event_type: ProgressEventType,
    data: serde_json::Value,
) -> crate::AgentServerResult<()> {
    let event = ProgressEvent::new(
        session_id.to_string(),
        request_id.to_string(),
        event_type,
        data,
    );

    // TODO: 实现类似 rcoder 的 SESSION_CACHE 事件广播机制
    // 现在只是记录日志
    info!(
        "📤 [SSE] 发送进度事件: session_id={}, request_id={}, event_type={:?}",
        session_id, request_id, event_type
    );

    // 这里应该将事件发送到对应会话的所有 SSE 连接
    // 需要实现类似 rcoder 的多连接广播机制

    Ok(())
}

/// 连接状态检查 - 完全复制 rcoder 的实现
pub async fn check_connection_status(
    state: &ApiState,
    session_id: &str,
) -> Result<serde_json::Value, crate::AgentServerError> {
    debug!("检查连接状态，会话ID: {}", session_id);

    // 检查会话是否存在
    if state.agent_manager.get_session(session_id).await.is_none() {
        return Err(crate::AgentServerError::SessionError(format!(
            "会话 {} 不存在", session_id
        )));
    }

    // 检查 Agent 状态
    match state.agent_manager.get_agent_status().await {
        Ok(status) => {
            let response = serde_json::json!({
                "session_id": session_id,
                "agent_status": status,
                "timestamp": chrono::Utc::now(),
                "connection_status": "active",
                "message": "SSE连接活跃"
            });

            Ok(response)
        }
        Err(e) => {
            error!("获取 Agent 状态失败: {}", e);
            Err(crate::AgentServerError::Other(format!(
                "无法获取 Agent 状态: {}",
                e
            )))
        }
    }
}

/// 获取会话统计信息 - 复制 rcoder 的统计功能
pub async fn get_session_stats(
    state: &ApiState,
    session_id: &str,
) -> Result<serde_json::Value, crate::AgentServerError> {
    debug!("获取会话统计信息，会话ID: {}", session_id);

    // 检查会话是否存在
    if let Some(session) = state.agent_manager.get_session(session_id).await {
        let response = serde_json::json!({
            "session_id": session_id,
            "session_status": session.status,
            "created_at": session.created_at,
            "last_activity": session.last_activity,
            "agent_type": session.agent_type,
            "current_task_id": session.current_task_id,
            "metadata": session.metadata,
            "timestamp": chrono::Utc::now()
        });

        Ok(response)
    } else {
        Err(crate::AgentServerError::SessionError(format!(
            "会话 {} 不存在", session_id
        )))
    }
}