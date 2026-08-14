//! UserApp workspace HTTP handlers（独立 `/api/userapp`，复用 `resolve_project`）。
//!
//! 响应格式：JSON 接口统一 `shared_types::HttpResult`（`{code, message, data, tid, success}`）；
//! SSE（logs/stream）与静态文件（static）为特殊通道，不包 HttpResult。
//!
//! 异步编译/发布（task 10-12）：
//! - `POST /build`：workspace 多项目打包（异步：返 taskId，进度经 task 流出）。
//! - `GET  /tasks/{taskId}`：查任务状态快照（轮询通道）。
//! - `GET  /tasks/{taskId}/logs`：查构建日志（分页，复用 `read_dev_log`）。
//! - `GET  /tasks/{taskId}/logs/stream`：任务进度 SSE（实时通道，进度事件推送）。
//! - `POST /tasks/{taskId}/cancel`：取消进行中的编译任务（软取消 + kill 进程组）。
//! - `GET  /static/{app_id}/{*rest}`：取整体包（复用 `serve_from_root` + COMPUTER_CORS）。
//!
//! 详见 `docs/application-management-service-v2-design.md` §5。

use std::convert::Infallible;
use std::time::Duration;

use async_stream::stream;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use garde::Validate;
use serde::{Deserialize, Serialize};
use shared_types::HttpResult;

use crate::AppState;
use crate::error::{AppError, AppResult};
use crate::extract::deserialize_id_string;
use crate::extract::deserialize_optional_id_string;
use crate::extract::{AppJson, AppPath, AppQuery};
use crate::service::dev_server::log::{ReadDevLogResult, read_dev_log};
use crate::service::userapp;
use crate::service::userapp::tasks::{BuildProgressEvent, BuildTaskSnapshot};

// ── HttpResult 转换层 ──────────────────────────────────────────────────────────

/// UserApp JSON 接口的统一响应：成功/失败都是 HttpResult shape + 语义 HTTP 状态码。
///
/// file-server 全局 AppError shape（`{success, code:"UNKNOWN_ERROR", error:{...}}`）服务于
/// TS 对齐路由不能全局改；UserApp 是 Rust 独有新业务（TS 无此路由），此处将 AppError
/// 映射为 HttpResult 错误（code/message + 4xx/5xx 状态码）。
pub(crate) enum UserAppReply<T> {
    Ok(T),
    Err(AppError),
}

impl<T: Serialize> IntoResponse for UserAppReply<T> {
    fn into_response(self) -> Response {
        use shared_types::error_codes as ec;
        match self {
            UserAppReply::Ok(data) => Json(HttpResult::success(data)).into_response(),
            UserAppReply::Err(e) => {
                let (code, status) = match &e {
                    AppError::Validation(..)
                    | AppError::ValidationI18n(..)
                    | AppError::Business(_) => (ec::ERR_VALIDATION, StatusCode::BAD_REQUEST),
                    AppError::Resource(_) => (ec::ERR_NOT_FOUND, StatusCode::NOT_FOUND),
                    AppError::Network(_) => (ec::ERR_SERVICE_UNAVAILABLE, StatusCode::BAD_GATEWAY),
                    AppError::Permission(_)
                    | AppError::System(_)
                    | AppError::File(_)
                    | AppError::Process(_) => (
                        ec::ERR_INTERNAL_SERVER_ERROR,
                        StatusCode::INTERNAL_SERVER_ERROR,
                    ),
                };
                let result = HttpResult::<T>::error(code, &e.to_string());
                (status, Json(result)).into_response()
            }
        }
    }
}

/// 便捷转换：`AppResult<T>` → `UserAppReply<T>`。
fn reply<T>(r: AppResult<T>) -> UserAppReply<T> {
    match r {
        Ok(data) => UserAppReply::Ok(data),
        Err(e) => UserAppReply::Err(e),
    }
}

// ── data DTO（HttpResult.data 载荷）───────────────────────────────────────────

/// build 响应 data（POST /build）。
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuildCreatedData {
    pub task_id: String,
    pub status: String,
}

/// cancel 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CancelData {
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub already_terminal: Option<bool>,
}

/// detect 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DetectData {
    pub detection: userapp::import::DetectionResult,
}

/// confirm 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfirmData {
    pub path: String,
}

/// `POST /api/userapp/build` 请求体。
#[derive(Debug, Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuildUserAppBody {
    /// UserApp 标识（= workspace app_id = file-server project_id）。
    #[serde(deserialize_with = "deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub app_id: String,
    /// 多租户三级目录（可选，留空走单级；对齐 resolve_project）。
    #[serde(default, deserialize_with = "deserialize_optional_id_string")]
    pub tenant_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_id_string")]
    pub space_id: Option<String>,
}

#[derive(Debug, Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportProjectBody {
    #[serde(deserialize_with = "deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub app_id: String,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub project_dir: String,
    #[serde(default, deserialize_with = "deserialize_optional_id_string")]
    pub tenant_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_id_string")]
    pub space_id: Option<String>,
}

/// 任务构建日志查询参数（`GET /tasks/{taskId}/logs`）。
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskLogsQuery {
    /// 子项目目录名（= service_id）；留空读 workspace 根日志目录。
    #[serde(default)]
    pub service: Option<String>,
    /// 起始行号（1-based，对齐 get-dev-log）。
    #[serde(default = "default_start_index")]
    pub start_index: usize,
}

fn default_start_index() -> usize {
    1
}

/// SSE 订阅参数（`GET /tasks/{taskId}/logs/stream`）。
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StreamQuery {
    /// 从哪个 seq 开始回放（0 = 从头；断线重连带上次最后 seq+1）。
    #[serde(default)]
    pub from_seq: u64,
}

/// `POST /api/userapp/build` —— 异步发起 workspace 打包，立即返 taskId。
///
/// 编译在后台 spawn 执行（`start_build_task`）；进度经 task 流出（轮询 `/tasks/{id}` +
/// SSE `/tasks/{id}/logs/stream`）。同 app_id 排队由 `BuildManager` per-project 互斥保证。
#[utoipa::path(
    post,
    path = "/build",
    request_body = BuildUserAppBody,
    responses((status = 200, body = HttpResult<BuildCreatedData>, description = "构建任务已创建")),
    tag = "UserApp"
)]
pub(crate) async fn build_workspace(
    State(state): State<AppState>,
    AppJson(body): AppJson<BuildUserAppBody>,
) -> UserAppReply<BuildCreatedData> {
    let result = async {
        body.validate().map_err(crate::error::from_garde)?;
        let task_id = userapp::start_build_task(
            &state.build_tasks,
            state.resolver.clone(),
            state.build_manager.clone(),
            body.app_id.clone(),
            body.tenant_id.clone(),
            body.space_id.clone(),
            state.config.dev_command_timeout_secs,
        )
        .await?;

        tracing::info!(app_id = %body.app_id, %task_id, "userapp build task started");
        Ok(BuildCreatedData {
            task_id,
            status: "pending".to_string(),
        })
    };
    reply(result.await)
}

#[utoipa::path(
    get,
    path = "/tasks/{task_id}",
    params(("task_id" = String, Path, description = "任务ID")),
    responses((status = 200, body = HttpResult<BuildTaskSnapshot>, description = "任务状态快照")),
    tag = "UserApp"
)]
pub(crate) async fn get_task(
    State(state): State<AppState>,
    AppPath(task_id): AppPath<String>,
) -> UserAppReply<BuildTaskSnapshot> {
    let result = async {
        let task = state
            .build_tasks
            .get(&task_id)
            .await
            .ok_or_else(|| AppError::resource(format!("build task not found: {task_id}")))?;
        Ok(task.snapshot().await)
    };
    reply(result.await)
}

/// `GET /api/userapp/tasks/{taskId}/logs` —— 构建日志分页（复用 `read_dev_log`）。
#[utoipa::path(
    get,
    path = "/tasks/{task_id}/logs",
    params(("task_id" = String, Path, description = "任务ID"), TaskLogsQuery),
    responses((status = 200, body = HttpResult<ReadDevLogResult>, description = "构建日志分页")),
    tag = "UserApp"
)]
pub(crate) async fn get_task_logs(
    State(state): State<AppState>,
    AppPath(task_id): AppPath<String>,
    AppQuery(q): AppQuery<TaskLogsQuery>,
) -> UserAppReply<ReadDevLogResult> {
    let result = async {
        let task = state
            .build_tasks
            .get(&task_id)
            .await
            .ok_or_else(|| AppError::resource(format!("build task not found: {task_id}")))?;
        let ws_root = task
            .workspace_root()
            .await
            .ok_or_else(|| AppError::resource("build task workspace not resolved yet"))?;
        let dir = match q.service.as_deref() {
            Some(service) if !service.is_empty() => {
                shared_types::validate_service_id(service).map_err(|error| {
                    AppError::validation(format!("invalid log service selector: {error}"))
                })?;
                crate::path_safety::ensure_within(&ws_root.join("logs"), service).map_err(|_| {
                    AppError::validation("log service selector escapes workspace logs")
                })?
            }
            _ => ws_root.join("logs"),
        };
        read_dev_log(&dir, q.start_index, "main", state.config.log_read_max_bytes).await
    };
    reply(result.await)
}

/// `GET /api/userapp/tasks/{taskId}/logs/stream` —— 任务进度 SSE（实时通道）。
///
/// 推送 `BuildProgressEvent`（event 名 = 事件类型，data = JSON 全量）；
/// 先回放 ring 里 `seq >= from_seq` 的历史，再实时跟随 broadcast，终态事件后关闭流。
#[utoipa::path(
    get,
    path = "/tasks/{task_id}/logs/stream",
    params(("task_id" = String, Path, description = "任务ID"), StreamQuery),
    responses(
        (status = 200, description = "SSE build progress stream", content_type = "text/event-stream"),
        (status = 404, description = "Task not found"),
    ),
    tag = "UserApp"
)]
pub(crate) async fn stream_task_logs(
    State(state): State<AppState>,
    AppPath(task_id): AppPath<String>,
    AppQuery(q): AppQuery<StreamQuery>,
) -> Response {
    // 错误路径 (task 不存在) 也走 HttpResult shape, 与同组 JSON 接口一致
    // (成功路径是 SSE 流, 豁免 HttpResult)
    let Some(task) = state.build_tasks.get(&task_id).await else {
        return UserAppReply::<()>::Err(AppError::resource(format!(
            "build task not found: {task_id}"
        )))
        .into_response();
    };
    let (replay, mut rx) = task.subscribe(q.from_seq).await;

    let progress = stream! {
        // 回放历史事件（seq >= from_seq）
        for (seq, ev) in replay {
            let terminal = is_terminal_event(&ev);
            yield Ok::<_, Infallible>(event_from_progress(seq, &ev));
            if terminal {
                return;
            }
        }
        // 实时跟随 broadcast
        loop {
            match rx.recv().await {
                Ok((seq, ev)) => {
                    let terminal = is_terminal_event(&ev);
                    yield Ok::<_, Infallible>(event_from_progress(seq, &ev));
                    if terminal {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    let data = serde_json::json!({"event":"stream_lagged","skipped":skipped});
                    yield Ok::<_, Infallible>(Event::default().event("stream_lagged").data(data.to_string()));
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(Box::pin(progress))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

/// `POST /api/userapp/tasks/{taskId}/cancel` —— 取消进行中的编译任务。
///
/// 双重取消：① 软取消（置 flag，build 循环服务间检查主动退出）；
/// ② 硬取消（kill 当前 build 子进程组，即时中断）。build 循环/错误分支随后 emit `Cancelled`。
#[utoipa::path(
    post,
    path = "/tasks/{task_id}/cancel",
    params(("task_id" = String, Path, description = "任务ID")),
    responses((status = 200, body = HttpResult<CancelData>, description = "取消结果")),
    tag = "UserApp"
)]
pub(crate) async fn cancel_task(
    State(state): State<AppState>,
    AppPath(task_id): AppPath<String>,
) -> UserAppReply<CancelData> {
    let result = async {
        let task = state
            .build_tasks
            .get(&task_id)
            .await
            .ok_or_else(|| AppError::resource(format!("build task not found: {task_id}")))?;
        if task.is_terminal().await {
            return Ok(CancelData {
                task_id,
                status: None,
                already_terminal: Some(true),
            });
        }
        task.cancel();
        // 硬 cancel：kill 当前 build 子进程组（run_command_to_log 用 process_group(0)，pid==pgid）。
        if let Some(pid) = task.pid() {
            let killed = crate::service::dev_server::process::kill_process_group(pid);
            tracing::info!(%task_id, pid, killed, "build task cancelled, process group signalled");
        } else {
            tracing::info!(%task_id, "build task cancelled (no active pid; soft cancel via loop check)");
        }
        // 主动 emit Cancelled：若 build 在循环间隙（非 build_generic 内），靠此置终态；
        // 若在 build_generic 内被 kill，错误分支的 is_cancelled 分支会 emit Cancelled
        //（终态保护丢弃这里的重复）。
        task.emit(BuildProgressEvent::Cancelled).await;
        Ok(CancelData {
            task_id,
            status: Some("cancelled".to_string()),
            already_terminal: None,
        })
    };
    reply(result.await)
}

#[utoipa::path(
    post,
    path = "/projects/detect",
    request_body = ImportProjectBody,
    responses((status = 200, body = HttpResult<DetectData>, description = "项目探测结果")),
    tag = "UserApp"
)]
pub(crate) async fn detect_project(
    State(state): State<AppState>,
    AppJson(body): AppJson<ImportProjectBody>,
) -> UserAppReply<DetectData> {
    let result = async {
        body.validate().map_err(crate::error::from_garde)?;
        let workspace = state
            .resolver
            .resolve_project(&crate::workspace::ProjectContext {
                project_id: body.app_id,
                tenant_id: body.tenant_id,
                space_id: body.space_id,
                isolation_type: None,
            })
            .await?;
        let detection = userapp::import::detect_project(&workspace, &body.project_dir).await?;
        Ok(DetectData { detection })
    };
    reply(result.await)
}

#[utoipa::path(
    post,
    path = "/projects/confirm",
    request_body = ImportProjectBody,
    responses((status = 200, body = HttpResult<ConfirmData>, description = "项目确认结果")),
    tag = "UserApp"
)]
pub(crate) async fn confirm_project(
    State(state): State<AppState>,
    AppJson(body): AppJson<ImportProjectBody>,
) -> UserAppReply<ConfirmData> {
    let result = async {
        body.validate().map_err(crate::error::from_garde)?;
        let app_id = body.app_id.clone();
        let workspace = state
            .resolver
            .resolve_project(&crate::workspace::ProjectContext {
                project_id: body.app_id,
                tenant_id: body.tenant_id,
                space_id: body.space_id,
                isolation_type: None,
            })
            .await?;
        let path = userapp::import::confirm_project(&workspace, &body.project_dir).await?;
        // workspace 级 git init（幂等）：本地版本管理 + publish snapshot commit 的前提。
        // 放 handler 层（持有 config.git_enabled / author）；失败仅告警，不阻断 manifest 确认。
        if state.config.git_enabled
            && let Err(e) = crate::service::git::write::init_repo(
                &workspace,
                &state.config.git_default_author_name,
                &state.config.git_default_author_email,
            )
        {
            tracing::warn!(%app_id, error = %e, "workspace git init failed (non-blocking)");
        }
        Ok(ConfirmData { path })
    };
    reply(result.await)
}

// ── SSE helper ──────────────────────────────────────────────────────────────────

/// `BuildProgressEvent` → SSE `Event`（event 名 = 事件类型，data = JSON 全量）。
fn event_from_progress(seq: u64, ev: &BuildProgressEvent) -> Event {
    let name = match ev {
        BuildProgressEvent::Stage { .. } => "stage",
        BuildProgressEvent::Building { .. } => "building",
        BuildProgressEvent::BuildOk { .. } => "build_ok",
        BuildProgressEvent::BuildFail { .. } => "build_fail",
        BuildProgressEvent::Log { .. } => "log",
        BuildProgressEvent::Completed { .. } => "completed",
        BuildProgressEvent::Failed { .. } => "failed",
        BuildProgressEvent::Cancelled => "cancelled",
    };
    let data = serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string());
    Event::default().id(seq.to_string()).event(name).data(data)
}

/// 是否终态事件（SSE 收到后关闭流）。
fn is_terminal_event(ev: &BuildProgressEvent) -> bool {
    matches!(
        ev,
        BuildProgressEvent::Completed { .. }
            | BuildProgressEvent::Failed { .. }
            | BuildProgressEvent::Cancelled
    )
}
