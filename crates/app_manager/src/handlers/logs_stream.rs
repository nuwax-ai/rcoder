//! 日志 WebSocket 流 handler（v2 §11）

use std::sync::Arc;

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{Path, Query, State},
    response::Response,
};
use serde::Deserialize;
use tracing::{info, instrument};
use utoipa::ToSchema;

use shared_types::AppError;

use super::state::AppManagerState;
use crate::models::LogEntry;

/// 日志流 query 参数
#[derive(Debug, Deserialize, Default, ToSchema)]
pub struct LogStreamQuery {
    /// 起始历史行数（默认 0 = 仅 follow 新行）
    pub tail: Option<u32>,
}

/// 日志 WebSocket 流（follow）。WS 升级后服务端逐行推送 `LogEntry` JSON；
/// 客户端断开 → 服务端停止 follow（receiver drop → runtime 任务终止）。
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/logs/stream",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("tail" = Option<u32>, Query, description = "起始历史行数（0=仅 follow 新行）")
    ),
    responses(
        (status = 200, description = "WebSocket 升级成功；逐行推送 LogEntry JSON（见 get_app_logs 响应体）")
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, ws))]
pub async fn stream_app_logs(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    ws: WebSocketUpgrade,
    Query(params): Query<LogStreamQuery>,
) -> Result<Response, AppError> {
    let tail = params.tail.unwrap_or(0);
    info!("[APP] log WS stream: {} (tail={})", app_id, tail);
    let mut rx = state.app_service.stream_app_logs(&app_id, tail).await?;
    Ok(ws.on_upgrade(move |mut socket: WebSocket| async move {
        while let Some(entry) = rx.recv().await {
            let json = serde_json::to_string(&LogEntry {
                timestamp: entry.timestamp.unwrap_or_default(),
                stream: entry.stream,
                message: entry.message,
            })
            .unwrap_or_default();
            if socket.send(Message::Text(json.into())).await.is_err() {
                break; // 客户端断开
            }
        }
    }))
}
