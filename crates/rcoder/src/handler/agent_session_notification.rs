//! agent 执行任务的时候 ,session_notification 的通知消息

use std::sync::Arc;

use axum::extract::{Path, State};
use serde_json::Value;

use crate::{AppError, HttpResult, router::AppState};

/// 健康检查端点
#[axum::debug_handler]
pub async fn agent_session_notification(
    State(_state): State<Arc<AppState>>,
    Path(_project_id): Path<String>,
) -> Result<HttpResult<Value>, AppError> {
    //todo! 把 SessionUpdate 结构体消息,通过SSE协议返回前端

    Ok(HttpResult::internal_error("not implemented"))
}
