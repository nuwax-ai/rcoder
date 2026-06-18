//! Computer Agent Permission Resolve Handler
//!
//! 处理 POST /computer/agent/permission/resolve 请求

use axum::{Json, extract::State};
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::http_server::router::AppState;
use crate::service::PERMISSION_MANAGER;
use shared_types::{
    AppError, HttpResult, ResolvePermissionHttpRequest, ResolvePermissionResponseDto,
};

/// 解析 Computer Agent 权限请求
pub async fn handle_computer_permission_resolve(
    State(_state): State<Arc<AppState>>,
    Json(request): Json<ResolvePermissionHttpRequest>,
) -> Result<Json<HttpResult<ResolvePermissionResponseDto>>, AppError> {
    debug!(
        "[HTTP] Permission resolve: session_id={:?}, tool_call_id={:?}",
        request.permission_resolve_request.session_id,
        request.permission_resolve_request.tool_call_id
    );

    let dto = request.to_dto();

    let result = PERMISSION_MANAGER.resolve_permission(dto).await;

    if result.success {
        info!(
            "[HTTP] Permission resolved: session_id={}, tool_call_id={}",
            result.session_id, result.tool_call_id
        );
    } else {
        warn!(
            "[HTTP] Permission resolve failed: session_id={}, error={:?}",
            result.session_id, result.error_code
        );
    }

    Ok(Json(HttpResult::success(result)))
}
