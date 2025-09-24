//! agent 执行任务的时候 ,session_notification 的通知消息
//!
//! 通过SSE协议将SessionUpdate消息实时推送给前端

use axum::{
    extract::Path,
    response::sse::{Event, Sse},
};
use futures::stream::{self, Stream};
use std::{convert::Infallible, time::Duration};
use tokio::time::sleep;
use tracing::{info, debug};


/// 建立SSE连接，实时推送该session的SessionUpdate消息
pub async fn agent_session_notification(
    Path(session_id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, crate::AppError> {

    info!("🔌 SSE连接建立: session_id={}", session_id);

    // 创建SSE流
    let stream = stream::unfold(session_id.clone(), move |session_id| {
        let session_id_clone = session_id.clone();
        async move {
            loop {
                // 获取并清空该session的消息
                let messages = crate::service::drain_session_messages(&session_id_clone
                );

                if !messages.is_empty() {
                    debug!("📤 推送 {} 条消息到 session: {}", messages.len(), session_id_clone);

                    // 逐条发送消息
                    for msg in messages {
                        let event = Event::default()
                            .event("session_update")
                            .data(serde_json::to_string(&msg).unwrap_or_else(|_| "{}".to_string()));

                        return Some((Ok(event), session_id_clone));
                    }
                }

                // 没有消息，等待一段时间再检查
                sleep(Duration::from_millis(100)).await;

                // 发送心跳保持连接（每30秒一次）
                // 注意：这里简化处理，实际可以用更复杂的心跳逻辑
                // 暂时通过定期重试来保持连接
            }
        }
    });

    Ok(Sse::new(stream))
}