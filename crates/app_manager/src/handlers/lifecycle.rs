//! 应用生命周期 handler（create / query / get / update / delete）

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
};
use tracing::{info, instrument};

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{
    AppRuntimeInfo, DeleteAppRequest, PaginatedResponse, QueryAppsRequest, UpdateAppRequest,
};

// create REST 面已删除（统一走 POST /{app_id}/start：不存在则由发布链/ url 部署自动创建）。

/// 查询应用列表
///
/// 实时查集群 + 过滤/分页；仅 status/app_ids 过滤生效。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/query",
    request_body = QueryAppsRequest,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<PaginatedResponse<AppRuntimeInfo>>),
        (status = 500, description = "集群查询失败", body = HttpResult<String>)
    ),
    tag = "UserApp · 生命周期"
)]
#[instrument(skip(state, request))]
pub async fn query_apps(
    State(state): State<Arc<AppManagerState>>,
    Json(request): Json<QueryAppsRequest>,
) -> Result<Json<HttpResult<PaginatedResponse<AppRuntimeInfo>>>, AppError> {
    info!("[APP] querying apps");
    let response = state.app_service.query_apps(request).await?;
    Ok(Json(HttpResult::success(response)))
}

/// 列出应用运行时状态
///
/// 对账接口：列出集群中所有 rcoder 托管的应用运行时状态，供 Java 在 rcoder/自身重启后对账（rcoder 不持久化 app 元数据）。
#[utoipa::path(
    get,
    path = "/api/v1/userapp/runtime",
    responses(
        (status = 200, description = "对账成功", body = HttpResult<Vec<AppRuntimeInfo>>),
        (status = 500, description = "集群查询失败", body = HttpResult<String>)
    ),
    tag = "UserApp · 生命周期"
)]
#[instrument(skip(state))]
pub async fn list_app_runtimes(
    State(state): State<Arc<AppManagerState>>,
) -> Result<Json<HttpResult<Vec<AppRuntimeInfo>>>, AppError> {
    info!("[APP] reconcile: listing all app runtimes");
    let runtimes = state.app_service.list_app_runtimes().await?;
    Ok(Json(HttpResult::success(runtimes)))
}

/// 获取应用运行时详情（实时查集群）
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    description = r#"
实时查询单个应用的运行时全量快照：phase / replicas / 健康状态（含实例 IP）、
端口与访问地址（`access.external.http` 等，Pingora 模式返回
`/proxy/userapp/prod/{user_id}/{app_id}` 形态）、release 与制品摘要。

- 与 `GET /runtime` 的区别：本接口是**单条实时**查询，对账列表是集群全量；
- 不存在 → 404；查询失败（集群不可达）→ 500。
"#,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<AppRuntimeInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "UserApp · 生命周期"
)]
#[instrument(skip(state))]
pub async fn get_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    info!("[APP] getting app runtime: {}", app_id);
    let runtime = state.app_service.get_app(&app_id).await?;
    Ok(Json(HttpResult::success(runtime)))
}

/// 更新应用（全量替换 desired state）
///
/// rcoder 无状态：调用方需发送完整新状态（`image` 必填）。K8s SSA re-apply 幂等，
/// Docker 重建容器；工作空间目录保留。详见设计文档 §5.2。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/update",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    request_body = UpdateAppRequest,
    responses(
        (status = 200, description = "更新成功", body = HttpResult<AppRuntimeInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "UserApp · 生产运维"
)]
#[instrument(skip(state, request))]
pub async fn update_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<UpdateAppRequest>,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    info!("[APP] updating app: {}", app_id);
    let runtime = state.app_service.update_app(&app_id, request).await?;
    Ok(Json(HttpResult::success(runtime)))
}

/// 删除应用
///
/// 删计算资源并注销运行态；默认保留持久存储，body `{"purge": true}` 连数据面
/// 一起清空。**仅 prod**：dev 开发环境的销毁走 storage 面的
/// `{env=dev}` destroy（builder 容器自愈重建语义不适合"删除"操作）。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{env}/delete",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("env" = String, Path, description = "目标环境：仅支持 `prod`（运行容器删除）")
    ),
    request_body = DeleteAppRequest,
    description = r#"
删除应用：停容器 → 注销 pingora backend → 删除 Deployment/Service/HTTPRoute
等计算资源（元数据行保留，误删找回可用）；`purge=true` 时连持久存储一起销毁。

- **仅 prod**：传 `env=dev` 返回 400——开发环境的销毁由 storage 面
  （`{env=dev}` destroy）承担，builder 容器常驻自愈无"删除"语义；
- Docker compose 下 purge 按 `user_id` 精确清理宿主机目录
  `prod/{user_id}/data/{app_id}` 分区（缺省回退归属元数据→通配兜底），
  **建议始终携带 user_id**；
- 乐观锁：`expected_resource_version` 不匹配 → 409。
"#,
    responses(
        (status = 200, description = "删除成功", body = HttpResult<String>),
        (status = 400, description = "env 非法或 dev 不支持 / user_id 非法", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 409, description = "resource_version 不匹配", body = HttpResult<String>)
    ),
    tag = "UserApp · 生产运维"
)]
#[instrument(skip(state, body))]
pub async fn delete_app(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, env)): Path<(String, String)>,
    body: Option<Json<DeleteAppRequest>>,
) -> Result<Json<HttpResult<String>>, AppError> {
    if shared_types::UserappEnv::parse(&env) != Some(shared_types::UserappEnv::Prod) {
        return Err(AppError::validation_error(
            "`delete` is a prod-runtime capability: pass env=prod (to tear down a dev environment use the storage destroy endpoint with env=dev)",
        ));
    }
    let (purge, user_id, expected_rv) = body
        .map(|Json(r)| {
            (
                r.purge.unwrap_or(false),
                r.user_id,
                r.expected_resource_version,
            )
        })
        .unwrap_or((false, None, None));
    // 显式 user_id 白名单校验后补录 owner 元数据（start 同款 best-effort——
    // 失败仅告警：后续 purge 的目录解析回退 metadata owner / 通配兜底）
    if let Some(uid) = user_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        shared_types::validate_identifier(uid, "user_id")
            .map_err(|e| AppError::validation_error(&e))?;
        if let Err(e) = state
            .app_service
            .record_dev_registration(&app_id, uid)
            .await
        {
            tracing::warn!(
                "[APP] delete owner registration failed (ignored): app_id={app_id}: {e}"
            );
        }
        info!(
            "[APP] deleting app: {} (purge={}, user_id={})",
            app_id, purge, uid
        );
    } else {
        info!("[APP] deleting app: {} (purge={})", app_id, purge);
    }
    state
        .app_service
        .delete_app(&app_id, purge, expected_rv.as_deref())
        .await?;
    Ok(Json(HttpResult::success("删除成功".to_string())))
}
