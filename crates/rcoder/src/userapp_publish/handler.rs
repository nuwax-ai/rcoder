//! UserApp 自动化构建发布 HTTP handler(rcoder 侧)。
//!
//! - `POST /api/v1/apps/{app_id}/publish` —— 一键 build + 发布(body 带 agent-runner `projectId`)。
//! - `POST /api/v1/apps/{app_id}/build`   —— 仅触发 agent-runner build + 透传进度。
//! - `GET  /publish/tasks/{taskId}`        —— 任务快照(轮询)。
//! - `GET  /publish/tasks/{taskId}/stream` —— 进度 SSE(回放 + 实时)。
//! - `POST /publish/tasks/{taskId}/cancel` —— 取消(透传到 agent-runner cancel + kill 进程组)。

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use container_runtime_api::ContainerCreateParams;
use serde::Deserialize;
use shared_types::error_codes::{
    ERR_INTERNAL_SERVER_ERROR, ERR_NOT_FOUND, ERR_TOO_MANY_REQUESTS, ERR_VALIDATION,
};
use shared_types::{ProjectAndContainerInfo, ServiceType};
use tracing::info;

use crate::AppError;
use crate::router::AppState;

use super::client;
use super::orchestrator;
use super::{
    CancelAttempt, PublishEvent, PublishTaskKind, PublishTaskSnapshot, PublishTaskStatus,
};

/// 路由聚合(注册到 rcoder 主 router,与 app_manager 路由同 `/api/v1/apps` 前缀)。
pub fn routes() -> axum::Router<Arc<AppState>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/api/v1/apps/{app_id}/ensure-builder", post(ensure_builder))
        .route("/api/v1/apps/{app_id}/publish", post(publish))
        .route("/api/v1/apps/{app_id}/build", post(build))
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
#[serde(rename_all = "camelCase")]
pub struct PublishBody {
    pub project_id: String,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct StreamQuery {
    #[serde(default)]
    pub from_seq: u64,
}

// ---- 类型化响应(Direction 2:保留 wire shape,仅消除 Json<Value>)----

/// publish / build 立即返回(task 已创建,后台 spawn)。
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublishTaskResponse {
    pub success: bool,
    pub task_id: String,
    pub status: String,
}

/// ensure-builder 返回。
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EnsureBuilderResponse {
    pub success: bool,
    pub app_id: String,
    pub container_name: String,
    pub container_ip: String,
}

/// get_task 返回(任务快照)。
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GetTaskResponse {
    pub success: bool,
    pub task: PublishTaskSnapshot,
}

/// cancel_task 返回(Accepted 时 already_terminal=None;AlreadyTerminal 时 Some(true))。
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CancelTaskResponse {
    pub success: bool,
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
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/publish",
    params(("app_id" = String, Path)),
    request_body = PublishBody,
    responses(
        (status = 200, body = PublishTaskResponse, description = "Publish task created"),
        (status = 400, description = "Invalid app_id / project_id"),
        (status = 404, description = "Agent-runner not found for project"),
        (status = 429, description = "Publish task capacity exhausted"),
        (status = 500, description = "Internal server error")
    ),
    tag = "UserApp 发布"
)]
pub async fn publish(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<PublishBody>,
) -> Result<Json<PublishTaskResponse>, AppError> {
    validate_publish_identifiers(&app_id, &body.project_id)?;
    let task = state
        .publish_tasks
        .create(
            app_id.clone(),
            body.project_id.clone(),
            PublishTaskKind::Publish,
        )
        .await
        .map_err(|error| too_many_requests(error.to_string()))?;
    let task_id = task.id.clone();
    let project_id = body.project_id.clone();
    tokio::spawn(async move {
        orchestrator::run_publish(task, state, project_id, app_id).await;
    });
    Ok(Json(PublishTaskResponse {
        success: true,
        task_id,
        status: "pending".into(),
    }))
}

/// `POST /api/v1/apps/{app_id}/build` —— 仅触发 agent-runner build(透传进度,不发布)。
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/build",
    params(("app_id" = String, Path)),
    request_body = PublishBody,
    responses(
        (status = 200, body = PublishTaskResponse, description = "Build task created"),
        (status = 400, description = "Invalid app_id / project_id"),
        (status = 404, description = "Agent-runner not found for project"),
        (status = 429, description = "Publish task capacity exhausted"),
        (status = 500, description = "Internal server error")
    ),
    tag = "UserApp 发布"
)]
pub async fn build(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<PublishBody>,
) -> Result<Json<PublishTaskResponse>, AppError> {
    validate_publish_identifiers(&app_id, &body.project_id)?;
    let task = state
        .publish_tasks
        .create(
            app_id.clone(),
            body.project_id.clone(),
            PublishTaskKind::Build,
        )
        .await
        .map_err(|error| too_many_requests(error.to_string()))?;
    let task_id = task.id.clone();
    let project_id = body.project_id.clone();
    tokio::spawn(async move {
        orchestrator::run_build(task, state, project_id, app_id).await;
    });
    Ok(Json(PublishTaskResponse {
        success: true,
        task_id,
        status: "pending".into(),
    }))
}

/// `POST /api/v1/apps/{app_id}/ensure-builder` —— 确保 UserAppBuilder pod 存在(幂等)。
///
/// 按 app_id 创建/复用 UserAppBuilder agent-runner pod(走 STS + per-app PVC
/// `rcoder-app-{app_id}-workspace`,复用 `dev-rcoder-agent-runner` 镜像),并注册到
/// `state.projects` 供 publish 流程的 `resolve_agent_addr` 据 app_id 定位。
///
/// 直接调 `runtime.create_container`(UserAppBuilder → `create_agent_container`),
/// **不走 ComputerContainerManager**(避免 ComputerAgentRunner 专属的 lazy_migrate)。
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/ensure-builder",
    params(("app_id" = String, Path)),
    responses((status = 200, body = EnsureBuilderResponse, description = "UserAppBuilder ensured")),
    tag = "UserApp 发布"
)]
pub async fn ensure_builder(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
) -> Result<Json<EnsureBuilderResponse>, AppError> {
    crate::handler::utils::validate_identifier(&app_id, "app_id")
        .map_err(|error| validation(error.to_string()))?;
    // UserAppBuilder identifier = project_id(app_id 兼任);host_workspace_path K8s 模式不用。
    let params = ContainerCreateParams::builder()
        .project_id(app_id.clone())
        .user_id(app_id.clone())
        .host_workspace_path("")
        .service_type(ServiceType::UserAppBuilder)
        .storage_size(DEFAULT_BUILDER_STORAGE_SIZE)
        .build();

    let container_info = state
        .runtime()
        .create_container(params)
        .await
        .map_err(|e| err(format!("ensure UserAppBuilder failed: {e}")))?;

    // 注册到 state.projects(publish 的 resolve_agent_addr 据 app_id 查 container_name/ip)。
    let project_info = if let Some(existing) = state.get_project(&app_id) {
        let mut info = (*existing).clone();
        info.set_container(Some(container_info.clone()));
        info
    } else {
        let mut info = ProjectAndContainerInfo::new(app_id.clone());
        info.set_service_type(Some(ServiceType::UserAppBuilder));
        info.set_container(Some(container_info.clone()));
        info
    };
    state
        .insert_project(app_id.clone(), Arc::new(project_info))
        .map_err(|e| {
            tracing::error!(error = %e, "[USERAPP_PUBLISH] register builder to projects failed");
            err(format!("register builder failed: {e}"))
        })?;

    info!(
        "[USERAPP_PUBLISH] UserAppBuilder ensured: app_id={}, container={}, ip={}",
        app_id, container_info.container_name, container_info.container_ip
    );

    Ok(Json(EnsureBuilderResponse {
        success: true,
        app_id,
        container_name: container_info.container_name,
        container_ip: container_info.container_ip,
    }))
}

/// UserAppBuilder per-app PVC 默认大小(后续可提到 config.yml 的 user-app-builder.service 段)。
const DEFAULT_BUILDER_STORAGE_SIZE: &str = "10Gi";

/// `GET /api/v1/apps/publish/tasks/{task_id}` —— 任务状态快照。
#[utoipa::path(
    get,
    path = "/api/v1/apps/publish/tasks/{task_id}",
    params(("task_id" = String, Path)),
    responses(
        (status = 200, body = GetTaskResponse, description = "Publish task snapshot"),
        (status = 404, description = "Publish task not found"),
        (status = 500, description = "Internal server error")
    ),
    tag = "UserApp 发布"
)]
pub async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<Json<GetTaskResponse>, AppError> {
    let task = state
        .publish_tasks
        .get(&task_id)
        .await
        .ok_or_else(|| not_found(format!("publish task not found: {task_id}")))?;
    let snapshot = task.snapshot().await;
    Ok(Json(GetTaskResponse {
        success: true,
        task: snapshot,
    }))
}

/// `GET /api/v1/apps/publish/tasks/{task_id}/stream` —— 进度 SSE(回放 + 实时,终态后关流)。
#[utoipa::path(
    get,
    path = "/api/v1/apps/publish/tasks/{task_id}/stream",
    params(("task_id" = String, Path), StreamQuery),
    responses((status = 200, description = "Publish progress SSE", content_type = "text/event-stream")),
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
        (status = 200, body = CancelTaskResponse, description = "Publish task cancellation accepted (or already terminal)"),
        (status = 404, description = "Publish task not found")
    ),
    tag = "UserApp 发布"
)]
pub async fn cancel_task(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<Json<CancelTaskResponse>, AppError> {
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
        CancelAttempt::AlreadyTerminal(status) => Ok(Json(CancelTaskResponse {
            success: true,
            task_id,
            already_terminal: Some(true),
            status,
        })),
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
            Ok(Json(CancelTaskResponse {
                success: true,
                task_id,
                already_terminal: None,
                status,
            }))
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
