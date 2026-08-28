//! 应用查询 handler（logs / health / stats / events / file-logs）

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
};
use garde::Validate as _;
use serde::Deserialize;
use tracing::{info, instrument};

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{HealthInfo, OwnerParams, ResourceStats};

/// 获取应用健康状态（由运行时状态派生）
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}/{app_stage}/health",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserAppBuilder）；`prod`=运行容器（UserApp）"),
        OwnerParams,
    ),
    description = r#"
轻量探活面，返回运行时状态派生的健康快照（`HealthInfo`），适合轮询面板 /
网关前置检查。

- `prod`：集群实时查询派生（phase、就绪探针结果、实例 IP）；不深入容器内，
  容器内服务的深检查由 app-cli :3010 `/health` 承担（经日志/代理面访问）。
- `dev`：探活开发容器内 file-server `/health`（builder 常驻自愈，不在则幂等重建）。
- 应用不存在（prod）→ 404；`app_stage` 非法 → 400。
"#,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<HealthInfo>),
        (status = 400, description = "app_stage 非法（仅 dev|prod）", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 生命周期"
)]
#[instrument(skip(state))]
pub async fn get_app_health(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    Query(owner): Query<OwnerParams>,
) -> Result<Json<HttpResult<HealthInfo>>, AppError> {
    owner
        .validate()
        .map_err(shared_types::garde_err_to_app_error)?;
    let app_stage = super::parse_app_stage_param(&app_stage)?;
    info!(
        "[APP] getting app health: {} app_stage={}",
        app_id,
        app_stage.as_str()
    );
    let health = state.app_service.get_app_health(app_stage, &app_id).await?;
    Ok(Json(HttpResult::success(health)))
}

/// stats 查询参数
///
/// `user_id` 必填：Docker compose 部署下按 owner 关联宿主机数据卷目录
/// （`prod/{user_id}/data/{app_id}` 分区）与调用侧映射关系维护；服务端
/// 当前用于审计留痕与 owner 一致性校验。
#[derive(Debug, Deserialize, utoipa::IntoParams, garde::Validate)]
#[into_params(parameter_in = Query)]
pub struct StatsParams {
    /// 所属用户 ID（必填；标识符白名单校验）
    #[garde(pattern(shared_types::IDENTIFIER_RE))]
    pub user_id: String,
}

/// 获取应用资源使用
///
/// best-effort：restart_count 来自运行时；CPU/内存需 metrics-server（Docker/
/// compose 形态无 metrics → 用量降级 0）。dev 环境按开发容器（双键标签定位）采集。
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}/{app_stage}/stats",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserAppBuilder）；`prod`=运行容器（UserApp）"),
        StatsParams
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<ResourceStats>),
        (status = 400, description = "参数错误（user_id 缺失/非法）", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 生命周期"
)]
#[instrument(skip(state, params))]
pub async fn get_app_stats(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    Query(params): Query<StatsParams>,
) -> Result<Json<HttpResult<ResourceStats>>, AppError> {
    let app_stage = super::parse_app_stage_param(&app_stage)?;
    // 标识符白名单校验（user_id 进宿主机卷路径分区与审计留痕，含 `/` 即逃逸）
    params
        .validate()
        .map_err(shared_types::garde_err_to_app_error)?;
    info!(
        "[APP] getting app stats: {} (user_id={})",
        app_id, params.user_id
    );
    let stats = state.app_service.get_app_stats(app_stage, &app_id).await?;
    Ok(Json(HttpResult::success(stats)))
}

/// 获取应用事件
///
/// best-effort：当前返回空，TODO 接 K8s events。**仅 prod**——K8s Events 绑定
/// 运行容器 Deployment，dev 开发环境无对应能力。
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}/{app_stage}/events",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：仅支持 `prod`（运行容器 K8s Events）"),
        OwnerParams,
    ),
    description = r#"
查运行容器的 Kubernetes Events（Pod 调度 / 拉取 / 启动 / 崩溃事件），用于
启动失败与重启排障。Docker 形态返回空列表。

> **仅 prod**：传 `app_stage=dev` 返回 400（开发环境无 Events 能力面）。
"#,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<Vec<container_runtime_api::AppEventInfo>>),
        (status = 400, description = "app_stage 非法或 dev 不支持（本接口仅 prod）", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 生命周期"
)]
#[instrument(skip(state))]
pub async fn get_app_events(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    Query(owner): Query<OwnerParams>,
) -> Result<Json<HttpResult<Vec<container_runtime_api::AppEventInfo>>>, AppError> {
    if shared_types::UserappStage::parse(&app_stage) != Some(shared_types::UserappStage::Prod) {
        return Err(AppError::validation_error(
            "`events` is a prod-runtime capability: pass app_stage=prod (dev environment has no k8s events)",
        ));
    }
    owner
        .validate()
        .map_err(shared_types::garde_err_to_app_error)?;
    info!(
        "[APP] getting app events: {} (user_id={})",
        app_id, owner.user_id
    );
    let events = state.app_service.get_app_events(&app_id).await?;
    Ok(Json(HttpResult::success(events)))
}
