//! 应用查询 handler（logs / health / stats / events / file-logs）

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::Deserialize;
use tracing::{info, instrument};

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{HealthInfo, ResourceStats};

/// 获取应用健康状态（由运行时状态派生）
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}/health",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    description = r#"
轻量探活面：返回运行时状态派生的健康快照（`AppRuntimeInfo.health` 同构——
phase、就绪探针结果、实例 IP 等），适合轮询面板 / 网关前置检查。

- 不深入容器内探活；容器内服务的深检查由 app-cli :3010 `/health` 承担
  （经日志/代理面访问）；
- 应用不存在 → 404。
"#,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<HealthInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "UserApp · 生命周期"
)]
#[instrument(skip(state))]
pub async fn get_app_health(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<HealthInfo>>, AppError> {
    info!("[APP] getting app health: {}", app_id);
    let runtime = state.app_service.get_app(&app_id).await?;
    Ok(Json(HttpResult::success(runtime.health)))
}

/// stats 查询参数
///
/// `user_id` 必填：Docker compose 部署下按 owner 关联宿主机数据卷目录
/// （`prod/{user_id}/data/{app_id}` 分区）与调用侧映射关系维护；服务端
/// 当前用于审计留痕与 owner 一致性校验。
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StatsParams {
    /// 所属用户 ID（必填；标识符白名单校验）
    pub user_id: String,
}

/// 获取应用资源使用
///
/// best-effort：restart_count 来自运行时；CPU/内存需 metrics-server。
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}/stats",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        StatsParams
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<ResourceStats>),
        (status = 400, description = "参数错误（user_id 缺失/非法）", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "UserApp · 生命周期"
)]
#[instrument(skip(state, params))]
pub async fn get_app_stats(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Query(params): Query<StatsParams>,
) -> Result<Json<HttpResult<ResourceStats>>, AppError> {
    // 标识符白名单校验（user_id 进宿主机卷路径分区与审计留痕，含 `/` 即逃逸）
    shared_types::validate_identifier(&params.user_id, "user_id")
        .map_err(|e| AppError::validation_error(&e))?;
    info!(
        "[APP] getting app stats: {} (user_id={})",
        app_id, params.user_id
    );
    let stats = state.app_service.get_app_stats(&app_id).await?;
    Ok(Json(HttpResult::success(stats)))
}

/// 获取应用事件
///
/// best-effort：当前返回空，TODO 接 K8s events。
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}/events",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<Vec<container_runtime_api::AppEventInfo>>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "UserApp · 生命周期"
)]
#[instrument(skip(state))]
pub async fn get_app_events(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<Vec<container_runtime_api::AppEventInfo>>>, AppError> {
    info!("[APP] getting app events: {}", app_id);
    let events = state.app_service.get_app_events(&app_id).await?;
    Ok(Json(HttpResult::success(events)))
}
