//! 持久存储管理 handler（v2 §5.4）

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
};
use tracing::{info, instrument};

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{DestroyStorageRequest, PaginatedResponse, QueryStorageRequest, StorageInfo};

/// 查询应用持久存储状态
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}/{env}/storage",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("env" = String, Path, description = "目标环境：`dev`=开发容器开发卷（is_orphan=卷在而 builder 容器不在）/ `prod`=生产运行卷")
    ),
    description = r#"
查询单个应用持久存储（per-app PVC / Docker bind 卷）的状态与用量：容量、已用、
挂载形态与四目录分区（code/data/logs/build）。用于对接前的容量确认与运维巡检。

- 扩容走 `POST {app_id}/update` 的 `resources.storage`（只扩不缩，需 StorageClass
  `allowVolumeExpansion`）；
- 危险清理见 `storage/clear`（prod 可恢复 / dev=重置开发工作区）与
  `storage/destroy`（不可逆）；
- 跨应用批量对账用 `POST /api/v1/userapp/storage/{env}/query` 分页面。
"#,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<StorageInfo>),
        (status = 400, description = "env 非法", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 文件与存储"
)]
#[instrument(skip(state))]
pub async fn get_app_storage(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, env)): Path<(String, String)>,
) -> Result<Json<HttpResult<StorageInfo>>, AppError> {
    let env = shared_types::UserappEnv::parse(&env)
        .ok_or_else(|| AppError::bad_request(&shared_types::invalid_env_error(&env)))?;
    info!(
        "[APP] getting app storage: {} (env={})",
        app_id,
        env.as_str()
    );
    let info = state.app_service.get_app_storage(env, &app_id).await?;
    Ok(Json(HttpResult::success(info)))
}

/// 清空应用持久存储内容
///
/// prod：留 PVC，可恢复；仅当 app 已 delete 时允许，否则 409 INVALID_STATE。
/// dev：清空 workspace 内容、**留容器留卷**（"重置开发工作区"语义，幂等）。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{env}/storage/clear",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("env" = String, Path, description = "目标环境：`dev`=重置开发工作区（清内容留容器留卷）/ `prod`=清运行卷（K8s 删 PVC 重建空卷；Docker 清目录内容）")
    ),
    responses(
        (status = 200, description = "清空成功", body = HttpResult<String>),
        (status = 400, description = "env 非法", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 409, description = "prod 下应用仍存在，需先 delete", body = HttpResult<String>),
        (status = 502, description = "dev 下开发容器不可达（或容器内无 clear 端点——旧镜像需换代）", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 文件与存储"
)]
#[instrument(skip(state))]
pub async fn clear_app_storage(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, env)): Path<(String, String)>,
) -> Result<Json<HttpResult<String>>, AppError> {
    let env = shared_types::UserappEnv::parse(&env)
        .ok_or_else(|| AppError::bad_request(&shared_types::invalid_env_error(&env)))?;
    info!(
        "[APP] clearing app storage: {} (env={})",
        app_id,
        env.as_str()
    );
    state.app_service.clear_app_storage(env, &app_id).await?;
    Ok(Json(HttpResult::success("存储已清空".to_string())))
}

/// 销毁应用持久存储
///
/// prod：销毁运行卷 PVC——高危·不可逆·释放配额；需 body `confirm=app_id`，仅 app
/// 已 delete 后允许。dev：销毁**整个开发环境**（builder 容器+dev 卷+目录，等价
/// 开发资源四步回收）；不动 owner 元数据（create-workspace 幂等重建）。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{env}/storage/destroy",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("env" = String, Path, description = "目标环境：`dev`=销毁整个开发环境（容器+卷+目录）/ `prod`=销毁运行卷 PVC")
    ),
    request_body = DestroyStorageRequest,
    responses(
        (status = 200, description = "已销毁", body = HttpResult<String>),
        (status = 400, description = "confirm 缺失/不匹配 app_id / env 非法", body = HttpResult<String>),
        (status = 409, description = "prod 下应用仍存在，需先 delete", body = HttpResult<String>),
        (status = 500, description = "PVC 卡 Terminating，需运维介入（pvc-protection finalizer 未移除）", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 文件与存储"
)]
#[instrument(skip(state, req))]
pub async fn destroy_app_storage(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, env)): Path<(String, String)>,
    Json(req): Json<DestroyStorageRequest>,
) -> Result<Json<HttpResult<String>>, AppError> {
    let env = shared_types::UserappEnv::parse(&env)
        .ok_or_else(|| AppError::bad_request(&shared_types::invalid_env_error(&env)))?;
    info!(
        "[APP] destroying app storage: {} (env={})",
        app_id,
        env.as_str()
    );
    state
        .app_service
        .destroy_app_storage(env, &app_id, &req.confirm)
        .await?;
    Ok(Json(HttpResult::success(
        "存储已销毁，配额已释放".to_string(),
    )))
}

/// 分页查询持久存储（强制分页，无全量模式；prod=运行卷清单，dev=开发卷清单）
#[utoipa::path(
    post,
    path = "/api/v1/userapp/storage/{env}/query",
    params(
        ("env" = String, Path, description = "目标环境：`dev`=开发卷清单（orphan=卷在而 builder 容器不在）/ `prod`=运行卷清单")
    ),
    request_body = QueryStorageRequest,
    description = r#"
管理面分页对账：列出全部应用在指定环境的持久存储状态（同单条
`/{app_id}/{env}/storage` 的 `StorageInfo` 结构，含容量/用量/挂载形态）。

- 强制分页：body 传 `page`（1 起）+ `page_size`；返回 `total` + 当前页条目，
  无"一次拉全量"模式（保护集群规模下的查询压力）；
- 常用过滤维度以 `QueryStorageRequest` 字段为准（Swagger 内展开可见）。
"#,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<PaginatedResponse<StorageInfo>>),
        (status = 400, description = "分页参数错误 / env 非法", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 文件与存储"
)]
#[instrument(skip(state, request))]
pub async fn query_storage(
    State(state): State<Arc<AppManagerState>>,
    Path(env): Path<String>,
    Json(request): Json<QueryStorageRequest>,
) -> Result<Json<HttpResult<PaginatedResponse<StorageInfo>>>, AppError> {
    let env = shared_types::UserappEnv::parse(&env)
        .ok_or_else(|| AppError::bad_request(&shared_types::invalid_env_error(&env)))?;
    info!(
        "[APP] querying storage list (env={}): page={} page_size={}",
        env.as_str(),
        request.page,
        request.page_size
    );
    let resp = state.app_service.query_storage(env, request).await?;
    Ok(Json(HttpResult::success(resp)))
}
