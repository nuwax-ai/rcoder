//! UserApp workspace HTTP handlers（独立 `/api/userapp`，复用 `resolve_project`）。
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
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::AppError;
use crate::extract::deserialize_id_string;
use crate::extract::deserialize_optional_id_string;
use crate::extract::{AppJson, AppPath, AppQuery};
use crate::service::dev_server::log::read_dev_log;
use crate::service::userapp;
use crate::service::userapp::tasks::BuildProgressEvent;

/// `POST /api/userapp/build` 请求体。
#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuildUserAppBody {
    /// UserApp 标识（= workspace app_id = file-server project_id）。
    #[serde(deserialize_with = "deserialize_id_string")]
    pub app_id: String,
    /// 多租户三级目录（可选，留空走单级；对齐 resolve_project）。
    #[serde(default, deserialize_with = "deserialize_optional_id_string")]
    pub tenant_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_id_string")]
    pub space_id: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportProjectBody {
    #[serde(deserialize_with = "deserialize_id_string")]
    pub app_id: String,
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
    responses(crate::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn build_workspace(
    State(state): State<AppState>,
    AppJson(body): AppJson<BuildUserAppBody>,
) -> Result<Json<Value>, AppError> {
    let task_id = userapp::start_build_task(
        &state.build_tasks,
        state.resolver.clone(),
        state.build_manager.clone(),
        body.app_id.clone(),
        body.tenant_id.clone(),
        body.space_id.clone(),
        state.config.dev_command_timeout_secs,
    )
    .await;

    tracing::info!(app_id = %body.app_id, %task_id, "userapp build task started");

    Ok(Json(json!({
        "success": true,
        "taskId": task_id,
        "status": "pending",
    })))
}

#[utoipa::path(
    get,
    path = "/tasks/{task_id}",
    params(("task_id" = String, Path, description = "任务ID")),
    responses(crate::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn get_task(
    State(state): State<AppState>,
    AppPath(task_id): AppPath<String>,
) -> Result<Json<Value>, AppError> {
    let task = state
        .build_tasks
        .get(&task_id)
        .await
        .ok_or_else(|| AppError::resource(format!("build task not found: {task_id}")))?;
    let snapshot = task.snapshot().await;
    Ok(Json(json!({
        "success": true,
        "task": serde_json::to_value(&snapshot).unwrap_or(Value::Null),
    })))
}

/// `GET /api/userapp/tasks/{taskId}/logs` —— 构建日志分页（复用 `read_dev_log`）。
#[utoipa::path(
    get,
    path = "/tasks/{task_id}/logs",
    params(("task_id" = String, Path, description = "任务ID"), TaskLogsQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn get_task_logs(
    State(state): State<AppState>,
    AppPath(task_id): AppPath<String>,
    AppQuery(q): AppQuery<TaskLogsQuery>,
) -> Result<Json<Value>, AppError> {
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
        Some(s) if !s.is_empty() => ws_root.join("logs").join(s),
        _ => ws_root.join("logs"),
    };
    let result = read_dev_log(&dir, q.start_index, "main", state.config.log_read_max_bytes).await?;
    Ok(Json(json!({
        "success": true,
        "logs": serde_json::to_value(&result).unwrap_or(Value::Null),
    })))
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
) -> Result<impl IntoResponse, AppError> {
    let task = state
        .build_tasks
        .get(&task_id)
        .await
        .ok_or_else(|| AppError::resource(format!("build task not found: {task_id}")))?;
    let (replay, mut rx) = task.subscribe(q.from_seq).await;

    let progress = stream! {
        // 回放历史事件（seq >= from_seq）
        for (_seq, ev) in replay {
            let terminal = is_terminal_event(&ev);
            yield Ok::<_, Infallible>(event_from_progress(&ev));
            if terminal {
                return;
            }
        }
        // 实时跟随 broadcast
        while let Ok(ev) = rx.recv().await {
            let terminal = is_terminal_event(&ev);
            yield Ok::<_, Infallible>(event_from_progress(&ev));
            if terminal {
                break;
            }
        }
    };

    Ok(Sse::new(Box::pin(progress)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

/// `POST /api/userapp/tasks/{taskId}/cancel` —— 取消进行中的编译任务。
///
/// 双重取消：① 软取消（置 flag，build 循环服务间检查主动退出）；
/// ② 硬取消（kill 当前 build 子进程组，即时中断）。build 循环/错误分支随后 emit `Cancelled`。
#[utoipa::path(
    post,
    path = "/tasks/{task_id}/cancel",
    params(("task_id" = String, Path, description = "任务ID")),
    responses(crate::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn cancel_task(
    State(state): State<AppState>,
    AppPath(task_id): AppPath<String>,
) -> Result<Json<Value>, AppError> {
    let task = state
        .build_tasks
        .get(&task_id)
        .await
        .ok_or_else(|| AppError::resource(format!("build task not found: {task_id}")))?;
    if task.is_terminal().await {
        return Ok(Json(json!({
            "success": true,
            "taskId": task_id,
            "alreadyTerminal": true,
        })));
    }
    task.cancel();
    // 硬 cancel：kill 当前 build 进程组（run_command_to_log 用 process_group(0)，pid==pgid）。
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
    Ok(Json(json!({
        "success": true,
        "taskId": task_id,
        "status": "cancelled",
    })))
}

#[utoipa::path(
    post,
    path = "/projects/detect",
    request_body = ImportProjectBody,
    responses(crate::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn detect_project(
    State(state): State<AppState>,
    AppJson(body): AppJson<ImportProjectBody>,
) -> Result<Json<Value>, AppError> {
    let workspace = state
        .resolver
        .resolve_project(&crate::workspace::ProjectContext {
            project_id: body.app_id,
            tenant_id: body.tenant_id,
            space_id: body.space_id,
            isolation_type: None,
        })
        .await?;
    let result = userapp::import::detect_project(&workspace, &body.project_dir).await?;
    Ok(Json(json!({"success": true, "detection": result})))
}

#[utoipa::path(
    post,
    path = "/projects/confirm",
    request_body = ImportProjectBody,
    responses(crate::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn confirm_project(
    State(state): State<AppState>,
    AppJson(body): AppJson<ImportProjectBody>,
) -> Result<Json<Value>, AppError> {
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
    Ok(Json(json!({"success": true, "path": path})))
}

// ── SSE helper ──────────────────────────────────────────────────────────────────

/// `BuildProgressEvent` → SSE `Event`（event 名 = 事件类型，data = JSON 全量）。
fn event_from_progress(ev: &BuildProgressEvent) -> Event {
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
    Event::default().event(name).data(data)
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
