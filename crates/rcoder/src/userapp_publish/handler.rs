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
use shared_types::error_codes::ERR_INTERNAL_SERVER_ERROR;
use shared_types::{ProjectAndContainerInfo, ServiceType};
use tracing::info;

use crate::AppError;
use crate::router::AppState;

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
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishBody {
    pub project_id: String,
}

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    #[serde(default)]
    pub from_seq: u64,
}

fn err(msg: impl Into<String>) -> AppError {
    AppError::with_message(ERR_INTERNAL_SERVER_ERROR, msg.into())
}

/// `POST /api/v1/apps/{app_id}/publish` —— 一键自动构建发布。
pub async fn publish(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<PublishBody>,
) -> Result<Json<Value>, AppError> {
    if body.project_id.trim().is_empty() {
        return Err(err(
            "projectId is required (agent-runner hosting the UserApp workspace)",
        ));
    }
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
pub async fn build(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<PublishBody>,
) -> Result<Json<Value>, AppError> {
    if body.project_id.trim().is_empty() {
        return Err(err(
            "projectId is required (agent-runner hosting the UserApp workspace)",
        ));
    }
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
pub async fn ensure_builder(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    if app_id.trim().is_empty() {
        return Err(err("appId is required"));
    }
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
pub async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let task = state
        .publish_tasks
        .get(&task_id)
        .await
        .ok_or_else(|| err(format!("publish task not found: {task_id}")))?;
    let snapshot = task.snapshot().await;
    Ok(Json(json!({
        "success": true,
        "task": serde_json::to_value(&snapshot).unwrap_or(Value::Null),
    })))
}

/// `GET /api/v1/apps/publish/tasks/{task_id}/stream` —— 进度 SSE(回放 + 实时,终态后关流)。
pub async fn stream_task(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
    Query(q): Query<StreamQuery>,
) -> Result<impl IntoResponse, AppError> {
    let task = state
        .publish_tasks
        .get(&task_id)
        .await
        .ok_or_else(|| err(format!("publish task not found: {task_id}")))?;
    let (replay, mut rx) = task.subscribe(q.from_seq).await;

    let progress = stream! {
        for (_seq, ev) in replay {
            let terminal = is_terminal(&ev);
            yield Ok::<_, Infallible>(event_from(&ev));
            if terminal {
                return;
            }
        }
        while let Ok(ev) = rx.recv().await {
            let terminal = is_terminal(&ev);
            yield Ok::<_, Infallible>(event_from(&ev));
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

/// `POST /api/v1/apps/publish/tasks/{task_id}/cancel` —— 取消(orchestrator 检测后调 agent-runner cancel)。
pub async fn cancel_task(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Result<Json<Value>, AppError> {
    let task = state
        .publish_tasks
        .get(&task_id)
        .await
        .ok_or_else(|| err(format!("publish task not found: {task_id}")))?;
    if task.is_terminal().await {
        return Ok(Json(json!({
            "success": true,
            "taskId": task_id,
            "alreadyTerminal": true,
        })));
    }
    task.cancel();
    Ok(Json(json!({
        "success": true,
        "taskId": task_id,
        "status": "cancelled",
    })))
}

/// PublishEvent → SSE Event(event 名 = 事件类型,data = JSON 全量)。
fn event_from(ev: &PublishEvent) -> Event {
    let name = match ev {
        PublishEvent::Stage { .. } => "stage",
        PublishEvent::BuildProgress { .. } => "build_progress",
        PublishEvent::Completed { .. } => "completed",
        PublishEvent::Failed { .. } => "failed",
        PublishEvent::Cancelled => "cancelled",
    };
    let data = serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string());
    Event::default().event(name).data(data)
}

fn is_terminal(ev: &PublishEvent) -> bool {
    matches!(
        ev,
        PublishEvent::Completed { .. } | PublishEvent::Failed { .. } | PublishEvent::Cancelled
    )
}
