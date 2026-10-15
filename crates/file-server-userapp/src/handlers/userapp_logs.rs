//! Development logs remain readable when management and business processes stop.
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::extract::{Path, State};
use axum::response::{
    IntoResponse, Response,
    sse::{Event, Sse},
};
use futures_util::StreamExt;
use shared_types::{HttpResult, LogQueryRequest, LogQueryResponse, LogSourceInfo};
use userapp_log_reader::{CancelOnDrop, LogProvider};

use crate::{
    UserAppError, UserAppState, extract::AppJson, service::logs::DevLogProvider, success_reply,
};
use file_server::error::AppError;

fn query_error(error: anyhow::Error) -> UserAppError {
    AppError::validation(format!("read development logs: {error:#}")).into()
}

/// 查询开发日志源。
///
/// 容器内只读文件，不要求业务或 app-cli 管理服务运行；目录描述异常在 diagnostic 返回。
#[utoipa::path(post, path = "/{app_id}/dev/logs/sources/query",
    params(("app_id" = String, Path, description = "应用 ID")), request_body = LogQueryRequest,
    responses((status = 200, body = HttpResult<Vec<LogSourceInfo>>, description = "日志源和匹配文件；诊断不影响其他可读源")),
    tag = "Userapp · dev · 日志查询")]
pub async fn query_sources(
    State(state): State<UserAppState>,
    Path(app_id): Path<String>,
    AppJson(request): AppJson<LogQueryRequest>,
) -> Result<axum::Json<HttpResult<Vec<LogSourceInfo>>>, UserAppError> {
    let logs = DevLogProvider::new(&state.fs.config, &app_id)?
        .load()
        .await
        .map_err(query_error)?;
    Ok(success_reply(
        logs.sources(request).await.map_err(query_error)?,
    ))
}

/// 查询开发日志快照。
///
/// 停服、启动失败和管理端口不可达时仍读取磁盘日志；不会启动或恢复任何业务进程。
#[utoipa::path(post, path = "/{app_id}/dev/logs/query",
    params(("app_id" = String, Path, description = "应用 ID")), request_body = LogQueryRequest,
    responses((status = 200, body = HttpResult<LogQueryResponse>, description = "日志、source_errors 和可续拉 cursor")),
    tag = "Userapp · dev · 日志查询")]
pub async fn query_logs(
    State(state): State<UserAppState>,
    Path(app_id): Path<String>,
    AppJson(request): AppJson<LogQueryRequest>,
) -> Result<axum::Json<HttpResult<LogQueryResponse>>, UserAppError> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let _guard = CancelOnDrop(cancelled.clone());
    let logs = DevLogProvider::new(&state.fs.config, &app_id)?
        .load()
        .await
        .map_err(query_error)?;
    Ok(success_reply(
        logs.query_with_cancel(request, cancelled)
            .await
            .map_err(query_error)?,
    ))
}

/// 订阅开发日志流。
///
/// 只读目录每 500ms 刷新；支持 log、source_error、source_recovered、cursor_reset、checkpoint 和 15s heartbeat。
/// 断线通过请求 cursor 续拉，流丢弃即取消文件读取。
#[utoipa::path(post, path = "/{app_id}/dev/logs/stream",
    params(("app_id" = String, Path, description = "应用 ID")), request_body = LogQueryRequest,
    responses((status = 200, description = "SSE: log, source_error, source_recovered, cursor_reset, checkpoint, heartbeat；cursor 断点续拉")),
    tag = "Userapp · dev · 日志查询")]
pub async fn stream_logs(
    State(state): State<UserAppState>,
    Path(app_id): Path<String>,
    AppJson(request): AppJson<LogQueryRequest>,
) -> Result<Response, UserAppError> {
    let provider: Arc<dyn LogProvider> = Arc::new(DevLogProvider::new(&state.fs.config, &app_id)?);
    provider
        .load()
        .await
        .map_err(query_error)?
        .sources(request.clone())
        .await
        .map_err(query_error)?;
    let stream = userapp_log_reader::stream(provider, request).map(|event| {
        let name = event.name();
        match event.data() {
            Ok(data) => Ok::<Event, Infallible>(Event::default().event(name).data(data)),
            Err(error) => Ok(Event::default().event("source_error").data(serde_json::json!({
                "service_id": "workspace", "source_id": "stream", "code": "serialization_failed", "message": error.to_string(),
            }).to_string())),
        }
    });
    Ok(Sse::new(stream).into_response())
}
