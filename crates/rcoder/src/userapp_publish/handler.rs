//! UserApp 自动化构建发布 HTTP handler(rcoder 侧)。
//!
//! - `POST /api/v1/apps/{app_id}/publish` —— 一键 build + 发布(body 带 agent-runner `projectId`)。
//! - `POST /api/v1/apps/{app_id}/build`   —— 仅触发 agent-runner build + 透传进度。
//!   两者都会自动 ensure UserAppBuilder(未注册时创建,orchestrator EnsureBuilder 阶段),
//!   调用方一次调用即可,无需先建 builder。
//! - `POST /publish/tasks/query`           —— 任务列表分页查询(免调用方自记 task_id)。
//! - `GET  /publish/tasks/{taskId}`        —— 任务快照(轮询)。
//! - `GET  /publish/tasks/{taskId}/stream` —— 进度 SSE(回放 + 实时)。
//! - `POST /publish/tasks/{taskId}/cancel` —— 取消(透传到 agent-runner cancel + kill 进程组)。

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use app_manager::{PaginatedResponse, Pagination};
use async_stream::stream;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use rcoder_storage::publish_repo::PublishTaskQuery;
use serde::Deserialize;
use shared_types::HttpResult;
use shared_types::error_codes::{
    ERR_INTERNAL_SERVER_ERROR, ERR_NOT_FOUND, ERR_TOO_MANY_REQUESTS, ERR_VALIDATION,
};

use crate::AppError;
use crate::router::AppState;

use super::client;
use super::orchestrator;
use super::types::PublishTaskStoreError;
use super::{CancelAttempt, PublishEvent, PublishTaskKind, PublishTaskSnapshot, PublishTaskStatus};

/// 路由聚合(注册到 rcoder 主 router,与 app_manager 路由同 `/api/v1/apps` 前缀)。
pub fn routes() -> axum::Router<Arc<AppState>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/api/v1/apps/{app_id}/publish", post(publish))
        .route("/api/v1/apps/{app_id}/build", post(build))
        .route("/api/v1/apps/publish/tasks/query", post(query_tasks))
        .route("/api/v1/apps/publish/tasks/{task_id}", get(get_task))
        .route(
            "/api/v1/apps/publish/tasks/{task_id}/stream",
            get(stream_task),
        )
        .route(
            "/api/v1/apps/publish/tasks/{task_id}/cancel",
            post(cancel_task),
        )
}

/// publish / build 请求体:agent-runner project_id(定位 build 目标)。
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct PublishBody {
    pub project_id: String,
}

/// tasks/query 请求体(分页 + 可选过滤;POST body 承载,与 /apps/query 惯例一致)。
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct QueryPublishTasksRequest {
    /// 页码,从 1 起,默认 1
    pub page: Option<u32>,
    /// 每页数量,1..=100,默认 20
    pub page_size: Option<u32>,
    /// 过滤条件
    pub filters: Option<PublishTaskFilters>,
}

/// 任务过滤(app_ids 精确集合 / kind / 只看未终态)。
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct PublishTaskFilters {
    /// 按 app_id 集合过滤(None=全部)
    pub app_ids: Option<Vec<String>>,
    /// build | publish(None=全部)
    pub kind: Option<PublishTaskKind>,
    /// 只看未终态任务(对账:该 app 现在有没有在跑的任务)
    pub active_only: Option<bool>,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct StreamQuery {
    #[serde(default)]
    pub from_seq: u64,
}

// ---- 类型化响应(HttpResult.data 载荷;错误链已是 HttpResult shape 零改动)----

/// publish / build 立即返回(task 已创建,后台 spawn)。
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct PublishTaskData {
    pub task_id: String,
    pub status: String,
}

/// get_task 返回(任务快照)。
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct GetTaskData {
    pub task: PublishTaskSnapshot,
}

/// cancel_task 返回(Accepted 时 already_terminal=None;AlreadyTerminal 时 Some(true))。
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct CancelTaskData {
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub already_terminal: Option<bool>,
    pub status: PublishTaskStatus,
}

fn err(msg: impl Into<String>) -> AppError {
    AppError::with_message(ERR_INTERNAL_SERVER_ERROR, msg.into())
}

fn validation(msg: impl Into<String>) -> AppError {
    AppError::with_message(ERR_VALIDATION, msg.into())
}

fn not_found(msg: impl Into<String>) -> AppError {
    AppError::with_message(ERR_NOT_FOUND, msg.into())
}

fn too_many_requests(msg: impl Into<String>) -> AppError {
    AppError::with_message(ERR_TOO_MANY_REQUESTS, msg.into())
}

fn conflict(msg: impl Into<String>) -> AppError {
    AppError::with_message(shared_types::error_codes::ERR_CONFLICT, msg.into())
}

/// publish/build 建任务失败映射:AppBusy(同 app 已有活跃任务)→ 409(U2 早拒绝);
/// CapacityExceeded(全局容量)→ 429;Backend(PG 持久化故障)→ 500。
fn create_task_error(error: PublishTaskStoreError) -> AppError {
    match error {
        PublishTaskStoreError::AppBusy { .. } => conflict(error.to_string()),
        PublishTaskStoreError::CapacityExceeded { .. } => too_many_requests(error.to_string()),
        PublishTaskStoreError::Backend(message) => err(message),
    }
}

fn validate_publish_identifiers(app_id: &str, project_id: &str) -> Result<(), AppError> {
    crate::handler::utils::validate_identifier(app_id, "app_id")
        .map_err(|error| validation(error.to_string()))?;
    crate::handler::utils::validate_identifier(project_id, "project_id")
        .map_err(|error| validation(error.to_string()))?;
    if app_id != project_id {
        return Err(validation(
            "projectId must equal appId because each UserAppBuilder owns one app workspace",
        ));
    }
    Ok(())
}

/// `POST /api/v1/apps/{app_id}/publish` —— 一键自动构建发布。
///
/// UserAppBuilder 自动 ensure:未注册(含 rcoder 重启后注册丢失)时创建并注册,
/// 调用方无需先建 builder;ensure 过程经 SSE `stage=EnsureBuilder` 可见,失败以任务
/// `failed` 终态呈现。
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/publish",
    params(("app_id" = String, Path)),
    request_body = PublishBody,
    responses(
        (status = 200, body = HttpResult<PublishTaskData>, description = "Publish task created"),
        (status = 400, description = "Invalid app_id / project_id"),
        (status = 409, description = "App already has an active publish/build task"),
        (status = 429, description = "Publish task capacity exhausted"),
        (status = 500, description = "Internal server error")
    ),
    tag = "UserApp 发布"
)]
pub async fn publish(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<PublishBody>,
) -> Result<Json<HttpResult<PublishTaskData>>, AppError> {
    validate_publish_identifiers(&app_id, &body.project_id)?;
    let task = state
        .publish_tasks
        .create(
            app_id.clone(),
            body.project_id.clone(),
            PublishTaskKind::Publish,
        )
        .await
        .map_err(create_task_error)?;
    let task_id = task.id.clone();
    let project_id = body.project_id.clone();
    tokio::spawn(async move {
        // panic 兜底：spawn 的 JoinHandle 被丢弃，run_* 内部 panic 会被 tokio 静默
        // 吞掉——任务停在 running 永不收敛，该 app 被 AppBusy 锁死直到重启。
        // catch_unwind 后 emit Failed（emit 自带终态守卫，已终态时为 no-op）。
        use futures::FutureExt as _;
        let outcome = std::panic::AssertUnwindSafe(orchestrator::run_publish(
            task.clone(),
            state,
            project_id,
            app_id,
        ))
        .catch_unwind()
        .await;
        if let Err(panic) = outcome {
            let detail = panic_message(&panic);
            tracing::error!("[USERAPP_PUBLISH] publish orchestration panicked: {detail}");
            task.emit(PublishEvent::Failed {
                error: format!("internal panic: {detail}"),
            })
            .await;
        }
    });
    Ok(Json(HttpResult::success(PublishTaskData {
        task_id,
        status: "pending".into(),
    })))
}

/// `POST /api/v1/apps/{app_id}/build` —— 仅触发 agent-runner build(透传进度,不发布)。
///
/// UserAppBuilder 自动 ensure(同 publish):未注册时创建并注册,调用方无需先建 builder;
/// ensure 过程经 SSE `stage=EnsureBuilder` 可见,失败以任务 `failed` 终态呈现。
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/build",
    params(("app_id" = String, Path)),
    request_body = PublishBody,
    responses(
        (status = 200, body = HttpResult<PublishTaskData>, description = "Build task created"),
        (status = 400, description = "Invalid app_id / project_id"),
        (status = 409, description = "App already has an active publish/build task"),
        (status = 429, description = "Publish task capacity exhausted"),
        (status = 500, description = "Internal server error")
    ),
    tag = "UserApp 发布"
)]
pub async fn build(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<PublishBody>,
) -> Result<Json<HttpResult<PublishTaskData>>, AppError> {
    validate_publish_identifiers(&app_id, &body.project_id)?;
    let task = state
        .publish_tasks
        .create(
            app_id.clone(),
            body.project_id.clone(),
            PublishTaskKind::Build,
        )
        .await
        .map_err(create_task_error)?;
    let task_id = task.id.clone();
    let project_id = body.project_id.clone();
    tokio::spawn(async move {
        // 同 publish：panic 兜底（见上方注释）
        use futures::FutureExt as _;
        let outcome = std::panic::AssertUnwindSafe(orchestrator::run_build(
            task.clone(),
            state,
            project_id,
            app_id,
        ))
        .catch_unwind()
        .await;
        if let Err(panic) = outcome {
            let detail = panic_message(&panic);
            tracing::error!("[USERAPP_PUBLISH] build orchestration panicked: {detail}");
            task.emit(PublishEvent::Failed {
                error: format!("internal panic: {detail}"),
            })
            .await;
        }
    });
    Ok(Json(HttpResult::success(PublishTaskData {
        task_id,
        status: "pending".into(),
    })))
}

/// `POST /api/v1/apps/publish/tasks/query` —— 任务列表分页查询。
///
/// 免去调用方自记 task_id。数据口径:PG 模式查 PG 行(覆盖多副本/rcoder 重启/内存
/// 容量驱逐,窗口=终态 24h TTL);Docker Compose(无 PG 单副本)遍历内存任务表,
/// rcoder 重启后列表为空。排序 `createdAt DESC, taskId DESC`。
#[utoipa::path(
    post,
    path = "/api/v1/apps/publish/tasks/query",
    request_body = QueryPublishTasksRequest,
    responses(
        (status = 200, body = HttpResult<PaginatedResponse<PublishTaskSnapshot>>, description = "Publish task page"),
        (status = 400, description = "Invalid page/pageSize or filters.appIds"),
        (status = 500, description = "Persistence backend error")
    ),
    tag = "UserApp 发布"
)]
pub async fn query_tasks(
    State(state): State<Arc<AppState>>,
    Json(request): Json<QueryPublishTasksRequest>,
) -> Result<Json<HttpResult<PaginatedResponse<PublishTaskSnapshot>>>, AppError> {
    let page = request.page.unwrap_or(1);
    let page_size = request.page_size.unwrap_or(20);
    if page < 1 {
        return Err(validation("page must be >= 1"));
    }
    if !(1..=100).contains(&page_size) {
        return Err(validation("pageSize must be within 1..=100"));
    }
    let filters = request.filters.unwrap_or_default();
    if let Some(app_ids) = &filters.app_ids {
        for app_id in app_ids {
            crate::handler::utils::validate_identifier(app_id, "filters.appIds")
                .map_err(|error| validation(error.to_string()))?;
        }
    }
    let query = PublishTaskQuery {
        app_ids: filters.app_ids,
        kind: filters.kind.map(|kind| kind.as_pg_str().to_string()),
        active_only: filters.active_only.unwrap_or(false),
    };
    let result = state
        .publish_tasks
        .list_snapshots(&query, page, page_size)
        .await
        .map_err(|error| err(error.to_string()))?;
    Ok(Json(HttpResult::success(PaginatedResponse {
        items: result.items,
        pagination: Pagination {
            page,
            page_size,
            total: result.total,
            total_pages: ((result.total as f64) / (page_size as f64)).ceil() as u32,
        },
    })))
}

/// `GET /api/v1/apps/publish/tasks/{task_id}` —— 任务状态快照。
#[utoipa::path(
    get,
    path = "/api/v1/apps/publish/tasks/{task_id}",
    params(("task_id" = String, Path)),
    responses(
        (status = 200, body = HttpResult<GetTaskData>, description = "Publish task snapshot"),
        (status = 404, description = "Publish task not found"),
        (status = 500, description = "Internal server error")
    ),
    tag = "UserApp 发布"
)]
pub async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<Json<HttpResult<GetTaskData>>, AppError> {
    // M5：内存未命中回查 PG 快照（跨重启/跨副本状态可查；活任务走内存含事件游标）。
    // 存储故障（Err）如实 500——此前 Backend 错误被吞成 404 "任务不存在"，
    // 误导调用方的重试/告警决策。
    let snapshot = state
        .publish_tasks
        .lookup_snapshot(&task_id)
        .await
        .map_err(|e| AppError::internal_server_error(&format!("publish task lookup failed: {e}")))?
        .ok_or_else(|| not_found(format!("publish task not found: {task_id}")))?;
    Ok(Json(HttpResult::success(GetTaskData { task: snapshot })))
}

/// `GET /api/v1/apps/publish/tasks/{task_id}/stream` —— 进度 SSE(回放 + 实时,终态后关流)。
#[utoipa::path(
    get,
    path = "/api/v1/apps/publish/tasks/{task_id}/stream",
    params(("task_id" = String, Path), StreamQuery),
    responses(
        (status = 200, description = "Publish progress SSE", content_type = "text/event-stream"),
        (status = 404, description = "Publish task not found"),
        (status = 500, description = "Internal server error")
    ),
    tag = "UserApp 发布"
)]
pub async fn stream_task(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
    Query(q): Query<StreamQuery>,
) -> Result<impl IntoResponse, AppError> {
    let task = state
        .publish_tasks
        .get(&task_id)
        .await
        .ok_or_else(|| not_found(format!("publish task not found: {task_id}")))?;
    let (replay, mut rx) = task.subscribe(q.from_seq).await;

    let progress = stream! {
        for (seq, ev) in replay {
            let terminal = is_terminal(&ev);
            yield Ok::<_, Infallible>(event_from(seq, &ev));
            if terminal {
                return;
            }
        }
        loop {
            match rx.recv().await {
                Ok((seq, ev)) => {
                    let terminal = is_terminal(&ev);
                    yield Ok::<_, Infallible>(event_from(seq, &ev));
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

    Ok(Sse::new(Box::pin(progress)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

/// `POST /api/v1/apps/publish/tasks/{task_id}/cancel` —— 取消(orchestrator 检测后调 agent-runner cancel)。
#[utoipa::path(
    post,
    path = "/api/v1/apps/publish/tasks/{task_id}/cancel",
    params(("task_id" = String, Path)),
    responses(
        (status = 200, body = HttpResult<CancelTaskData>, description = "Publish task cancellation accepted (or already terminal)"),
        (status = 404, description = "Publish task not found")
    ),
    tag = "UserApp 发布"
)]
pub async fn cancel_task(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<Json<HttpResult<CancelTaskData>>, AppError> {
    let task = state
        .publish_tasks
        .get(&task_id)
        .await
        .ok_or_else(|| not_found(format!("publish task not found: {task_id}")))?;
    // request_cancel 在 event_lock 内原子 check 终态 + 转 Cancelling,消除"check 与 emit 之间
    // 被 Completed 抢入却仍回 cancelled"的撒谎窗口(#5)。终态 Cancelled/Failed 由 orchestrator
    // 完成远端取消/回滚后 emit —— handler 不再抢先置终态(原行为会让回滚失败被顶层 !is_terminal
    // 守卫吞掉,#6)。
    match task.request_cancel().await {
        CancelAttempt::AlreadyTerminal(status) => Ok(Json(HttpResult::success(CancelTaskData {
            task_id,
            already_terminal: Some(true),
            status,
        }))),
        CancelAttempt::Accepted => {
            // 尽力通知 agent-runner 取消 build;失败仅 warn(orchestrator 仍会收敛终态)。
            if let Some(remote) = task.remote_build().await
                && let Err(error) = client::cancel_build(&remote.addr, &remote.task_id).await
            {
                tracing::warn!(
                    %task_id,
                    remote_task_id = %remote.task_id,
                    error = %error,
                    "publish task cancellation requested but remote build cancel failed"
                );
            }
            // 返回【实际】状态(此时为 cancelling),不再硬编码 "cancelled"。
            let status = task.status().await;
            Ok(Json(HttpResult::success(CancelTaskData {
                task_id,
                already_terminal: None,
                status,
            })))
        }
    }
}

/// PublishEvent → SSE Event(event 名 = 事件类型,data = JSON 全量)。
fn event_from(seq: u64, ev: &PublishEvent) -> Event {
    let name = match ev {
        PublishEvent::Stage { .. } => "stage",
        PublishEvent::BuildProgress { .. } => "build_progress",
        PublishEvent::Cancelling => "cancelling",
        PublishEvent::Completed { .. } => "completed",
        PublishEvent::Failed { .. } => "failed",
        PublishEvent::Cancelled => "cancelled",
    };
    let data = serde_json::to_string(ev).unwrap_or_else(|e| {
        tracing::error!(seq, error = %e, "serialize publish SSE event failed");
        "{}".to_string()
    });
    Event::default().id(seq.to_string()).event(name).data(data)
}

fn is_terminal(ev: &PublishEvent) -> bool {
    matches!(
        ev,
        PublishEvent::Completed { .. } | PublishEvent::Failed { .. } | PublishEvent::Cancelled
    )
}
/// panic payload → 可读信息（&str / String 直接取，其他类型给占位）。
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}
