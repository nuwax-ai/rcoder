//! 应用查询 handler（logs / health / stats / events / file-logs）

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use garde::Validate as _;
use serde::Deserialize;
use tracing::{info, instrument};

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{HealthInfo, ResourceStats};

/// 获取应用健康状态（由运行时状态派生）
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}/{app_stage}/health",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserappBuilder）；`prod`=运行容器（Userapp）"),
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
        (status = 200, description = "查询成功", body = HttpResult<HealthInfo>)
    ),
    tag = "Userapp · 双态 · 生命周期"
)]
#[instrument(skip(state))]
pub async fn get_app_health(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
) -> Result<Json<HttpResult<HealthInfo>>, AppError> {
    let app_stage = super::parse_app_stage_param(&app_stage)?;
    info!(
        "[APP] getting app health: {} app_stage={}",
        app_id,
        app_stage.as_str()
    );
    let health = state.app_service.get_app_health(app_stage, &app_id).await?;
    Ok(Json(HttpResult::success(health)))
}

/// 获取应用业务就绪状态（只读观察：服务健康契约 + Pingap 入口/生效配置）
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}/{app_stage}/readiness",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserappBuilder）；`prod`=运行容器（Userapp）"),
    ),
    description = r#"
业务就绪查询（只读）：由当前物理实例内 app-cli 的业务观察（服务 HTTP 健康契约
+ Pingap 入口/生效配置）合并平台控制意图后的快照。与容器探针（`/health`、
`/ready`）分离。

- 查询成功恒 200（`success=true` 只表示观察完成），**`data.ready` 才表示业务可用**；
  未就绪/启动中/已停止/失败/未知/不支持都是合法观察结果。
- `status`：`not_deployed` / `starting` / `stopping` / `stopped` / `ready` /
  `degraded` / `failed` / `unknown` / `unsupported`；`reason_code` 为结构化原因。
- 只读保证：不启动/唤醒/停止容器，不刷新闲置计时，不阻塞 Stop/Restart。
- 查询期间停止已受理 → `stopping`；旧运行时无新接口 → `unsupported`
  （`RUNTIME_UPGRADE_REQUIRED`）。
- 应用权威记录不存在或已删除、`app_stage` 非法、查询系统故障分别走错误信封
  （404/400/5xx）。
"#,
    responses(
        (status = 200, description = "观察完成（data.ready 才是业务可用）", body = HttpResult<shared_types::UserAppReadinessResponse>)
    ),
    tag = "Userapp · 双态 · 生命周期"
)]
#[instrument(skip(state))]
pub async fn get_app_readiness(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
) -> Response {
    let app_stage = match super::parse_app_stage_param(&app_stage) {
        Ok(stage) => stage,
        Err(error) => return error.into_response(),
    };
    match state
        .app_service
        .get_app_readiness(app_stage, &app_id)
        .await
    {
        Ok(readiness) => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "no-store")],
            Json(HttpResult::success(readiness)),
        )
            .into_response(),
        Err(error) => AppError::from(error).into_response(),
    }
}

/// stats 查询参数
///
/// `user_id` 必填：宿主机数据卷分区归属目录名（Docker compose 挂载路径
/// `prod/{user_id}/data/{app_id}` 组成段）——容器未启动时按此自动唤醒后挂载。
#[derive(Debug, Deserialize, utoipa::IntoParams, garde::Validate)]
#[into_params(parameter_in = Query)]
pub struct StatsParams {
    /// 宿主机数据卷分区归属目录名（必填；Docker compose 挂载路径组成段——
    /// 容器未启动时按此自动唤醒后挂载）
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
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserappBuilder）；`prod`=运行容器（Userapp）"),
        StatsParams
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<ResourceStats>)
    ),
    tag = "Userapp · 双态 · 生命周期"
)]
#[instrument(skip(state, params))]
pub async fn get_app_stats(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    Query(params): Query<StatsParams>,
) -> Result<Json<HttpResult<ResourceStats>>, AppError> {
    let app_stage = super::parse_app_stage_param(&app_stage)?;
    // 标识符白名单校验（user_id 为宿主机卷路径分区组成段，含 `/` 即逃逸）
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
    ),
    description = r#"
查运行容器的 Kubernetes Events（Pod 调度 / 拉取 / 启动 / 崩溃事件），用于
启动失败与重启排障。Docker 形态返回空列表。

> **仅 prod**：传 `app_stage=dev` 返回 400（开发环境无 Events 能力面）。
"#,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<Vec<container_runtime_api::AppEventInfo>>)
    ),
    tag = "Userapp · 双态 · 生命周期"
)]
#[instrument(skip(state))]
pub async fn get_app_events(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
) -> Result<Json<HttpResult<Vec<container_runtime_api::AppEventInfo>>>, AppError> {
    if shared_types::UserappStage::parse(&app_stage) != Some(shared_types::UserappStage::Prod) {
        return Err(AppError::validation_error(
            "`events` is a prod-runtime capability: pass app_stage=prod (dev environment has no k8s events)",
        ));
    }
    info!("[APP] getting app events: {}", app_id);
    let events = state.app_service.get_app_events(&app_id).await?;
    Ok(Json(HttpResult::success(events)))
}
