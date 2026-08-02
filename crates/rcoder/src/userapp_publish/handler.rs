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
use serde_json::{Value, json};
use shared_types::error_codes::{ERR_INTERNAL_SERVER_ERROR, ERR_NOT_FOUND, ERR_VALIDATION};
use shared_types::{ProjectAndContainerInfo, ServiceType};
use tracing::info;

use crate::AppError;
use crate::router::AppState;

use super::client;
use super::orchestrator;
use super::task::{PublishEvent, PublishTaskKind};

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

fn err(msg: impl Into<String>) -> AppError {
    AppError::with_message(ERR_INTERNAL_SERVER_ERROR, msg.into())
}

fn validation(msg: impl Into<String>) -> AppError {
    AppError::with_message(ERR_VALIDATION, msg.into())
}

fn not_found(msg: impl Into<String>) -> AppError {
    AppError::with_message(ERR_NOT_FOUND, msg.into())
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
    responses((status = 200, description = "Publish task created")),
    tag = "UserApp 发布"
)]
pub async fn publish(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<PublishBody>,
) -> Result<Json<Value>, AppError> {
    validate_publish_identifiers(&app_id, &body.project_id)?;
    let task = state
        .publish_tasks
        .create(
            app_id.clone(),
            body.project_id.clone(),
            PublishTaskKind::Publish,
        )
        .await;
    let task_id = task.id.clone();
    let project_id = body.project_id.clone();
    tokio::spawn(async move {
        orchestrator::run_publish(task, state, project_id, app_id).await;
    });
    Ok(Json(json!({
        "success": true,
        "taskId": task_id,
        "status": "pending",
    })))
}

/// `POST /api/v1/apps/{app_id}/build` —— 仅触发 agent-runner build(透传进度,不发布)。
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/build",
    params(("app_id" = String, Path)),
    request_body = PublishBody,
    responses((status = 200, description = "Build task created")),
    tag = "UserApp 发布"
)]
pub async fn build(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<PublishBody>,
) -> Result<Json<Value>, AppError> {
    validate_publish_identifiers(&app_id, &body.project_id)?;
    let task = state
        .publish_tasks
        .create(
            app_id.clone(),
            body.project_id.clone(),
            PublishTaskKind::Build,
        )
        .await;
    let task_id = task.id.clone();
    let project_id = body.project_id.clone();
    tokio::spawn(async move {
        orchestrator::run_build(task, state, project_id, app_id).await;
    });
    Ok(Json(json!({
        "success": true,
        "taskId": task_id,
        "status": "pending",
    })))
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
    responses((status = 200, description = "UserAppBuilder ensured")),
    tag = "UserApp 发布"
)]
pub async fn ensure_builder(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
) -> Result<Json<Value>, AppError> {
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

    Ok(Json(json!({
        "success": true,
        "appId": app_id,
        "containerName": container_info.container_name,
        "containerIp": container_info.container_ip,
    })))
}

/// UserAppBuilder per-app PVC 默认大小(后续可提到 config.yml 的 user-app-builder.service 段)。
const DEFAULT_BUILDER_STORAGE_SIZE: &str = "10Gi";

/// `GET /api/v1/apps/publish/tasks/{task_id}` —— 任务状态快照。
#[utoipa::path(
    get,
    path = "/api/v1/apps/publish/tasks/{task_id}",
    params(("task_id" = String, Path)),
    responses((status = 200, description = "Publish task snapshot")),
    tag = "UserApp 发布"
)]
pub async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let task = state
        .publish_tasks
        .get(&task_id)
        .await
        .ok_or_else(|| not_found(format!("publish task not found: {task_id}")))?;
    let snapshot = task.snapshot().await;
    Ok(Json(json!({
        "success": true,
        "task": serde_json::to_value(&snapshot).unwrap_or(Value::Null),
    })))
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
    responses((status = 200, description = "Publish task cancelled")),
    tag = "UserApp 发布"
)]
pub async fn cancel_task(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let task = state
        .publish_tasks
        .get(&task_id)
        .await
        .ok_or_else(|| not_found(format!("publish task not found: {task_id}")))?;
    if task.is_terminal().await {
        return Ok(Json(json!({
            "success": true,
            "taskId": task_id,
            "alreadyTerminal": true,
        })));
    }
    task.cancel();
    task.emit(PublishEvent::Cancelled).await;
    if let Some(remote) = task.remote_build().await
        && let Err(error) = client::cancel_build(&remote.addr, &remote.task_id).await
    {
        tracing::warn!(
            %task_id,
            remote_task_id = %remote.task_id,
            error = %error,
            "publish task marked cancelled but remote build cancellation failed"
        );
    }
    Ok(Json(json!({
        "success": true,
        "taskId": task_id,
        "status": "cancelled",
    })))
}

/// PublishEvent → SSE Event(event 名 = 事件类型,data = JSON 全量)。
fn event_from(seq: u64, ev: &PublishEvent) -> Event {
    let name = match ev {
        PublishEvent::Stage { .. } => "stage",
        PublishEvent::BuildProgress { .. } => "build_progress",
        PublishEvent::Completed { .. } => "completed",
        PublishEvent::Failed { .. } => "failed",
        PublishEvent::Cancelled => "cancelled",
    };
    let data = serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string());
    Event::default().id(seq.to_string()).event(name).data(data)
}

fn is_terminal(ev: &PublishEvent) -> bool {
    matches!(
        ev,
        PublishEvent::Completed { .. } | PublishEvent::Failed { .. } | PublishEvent::Cancelled
    )
}
