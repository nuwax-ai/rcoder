//! 应用操作 handler（start / stop / restart）

use std::sync::Arc;

use axum::extract::{Json, Path, State};
use tracing::{info, instrument};

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{AppRuntimeInfo, RecyclePolicyRequest, StartAppRequest, StartAppResult};

/// 统一部署+启动入口（无 body = 传统启动；带 url = 轻量部署）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/start",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body(
        content = StartAppRequest,
        description = "全可选——无 body 或空对象 = 传统启动（scale=1）。带 url 触发轻量部署：下载 zip → prepare → activate（切流+等就绪，失败保留旧版本现场）→ 启动；release_id 缺省自动生成并在响应返回；sha256 可选校验；env/idle_timeout_seconds 覆盖；pg 凭据自动对齐（不一致重置，失败不阻断部署）"
    ),
    responses(
        (status = 200, description = "启动/部署成功", body = HttpResult<StartAppResult>),
        (status = 404, description = "应用不存在（无 url 传统启动）", body = HttpResult<String>),
        (status = 409, description = "release 幂等冲突（同 id 不同内容）", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn start_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    body: Option<Json<StartAppRequest>>,
) -> Result<Json<HttpResult<StartAppResult>>, AppError> {
    let request = body.map(|Json(b)| b).unwrap_or_default();
    info!(
        "[APP] starting app: {} (deploy={})",
        app_id,
        request.url.is_some()
    );
    let result = state
        .app_service
        .start_app_enhanced(&app_id, request)
        .await?;
    Ok(Json(HttpResult::success(result)))
}

/// 停止应用（scale replicas = 0）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/stop",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "停止成功", body = HttpResult<AppRuntimeInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn stop_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    info!("[APP] stopping app: {}", app_id);
    let runtime = state.app_service.stop_app(&app_id).await?;
    Ok(Json(HttpResult::success(runtime)))
}

/// 重启应用（rollout restart；可选参数与 start 同款——带 url 即部署新版本并重启）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/restart",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body(
        content = StartAppRequest,
        description = "全可选——无 body = 传统 rollout restart。带 url = 部署新版本（activate 自带切流）；其余字段语义同 start"
    ),
    responses(
        (status = 200, description = "重启/部署成功", body = HttpResult<StartAppResult>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn restart_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    body: Option<Json<StartAppRequest>>,
) -> Result<Json<HttpResult<StartAppResult>>, AppError> {
    let request = body.map(|Json(b)| b).unwrap_or_default();
    info!(
        "[APP] restarting app: {} (deploy={})",
        app_id,
        request.url.is_some()
    );
    let result = state
        .app_service
        .restart_app_enhanced(&app_id, request)
        .await?;
    Ok(Json(HttpResult::success(result)))
}

/// 设置闲置回收策略（动态、免重启：免费↔付费 tier 变更）
///
/// strategic-merge Deployment 注解,不碰 pod template → 不触发 rollout,下个扫描 tick 生效。
/// 比 update 轻（无需 image）。两字段皆 None → 400。
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/recycle-policy",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    request_body = RecyclePolicyRequest,
    responses(
        (status = 200, description = "策略已更新（免重启）", body = HttpResult<AppRuntimeInfo>),
        (status = 400, description = "参数错误（两字段皆空）", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, request))]
pub async fn set_recycle_policy(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<RecyclePolicyRequest>,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    let runtime = state
        .app_service
        .set_recycle_policy(&app_id, request)
        .await?;
    Ok(Json(HttpResult::success(runtime)))
}
