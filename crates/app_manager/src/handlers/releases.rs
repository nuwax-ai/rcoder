//! 应用发布 handler（prepare / activate / rollback / list / delete）
//!
//! 发布状态机：prepare（下载校验入库）→ activate（切流 + ensure 容器 + 等就绪，
//! 单接口收敛到 Active/Failed，失败**保留现场**）→ 失败后 rollback 显式恢复上一版本。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use shared_types::{AppError, HttpResult};

use crate::models::{
    ActivateReleaseRequest, PrepareReleaseRequest, ReleaseInfo, ReleaseListResponse,
    RollbackReleaseRequest,
};
use crate::release_flow::runtime::{MAX_READY_TIMEOUT_SECS, MIN_READY_TIMEOUT_SECS};

use super::AppManagerState;

/// 预备发布：从 URL 下载制品包，sha256/大小校验后入库（不切流）
///
/// 幂等：release_id 已存在且 sha256/size 一致时直接返回既有记录；
/// 不一致返回 409。制品存 PVC `releases/packages/`，索引写 `releases/index.json`。
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/releases/prepare",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body = PrepareReleaseRequest,
    responses(
        (status = 200, description = "预备成功（或幂等命中既有记录），status=Prepared", body = HttpResult<ReleaseInfo>),
        (status = 400, description = "参数校验失败（release_id/sha256 格式、retention 越界、URL 非法）", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 409, description = "release_id 已存在但 sha256/size 不一致", body = HttpResult<String>),
        (status = 500, description = "下载失败/校验失败/存储 IO 错误", body = HttpResult<String>)
    ),
    tag = "应用发布"
)]
pub async fn prepare_release(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<PrepareReleaseRequest>,
) -> Result<Json<HttpResult<ReleaseInfo>>, AppError> {
    Ok(Json(HttpResult::success(
        state.app_service.prepare_release(&app_id, request).await?,
    )))
}

/// 激活发布（单接口：切流 → ensure 运行容器 → 等就绪 → 提交/失败）
///
/// - 就绪 → Active（清 `.rollback` 快照、retention 清理）。
/// - 就绪超时/进入 Error → `Ok(status=Failed)` 且**保留现场**（code=失败版、`.rollback`=上一版、
///   制品包不动，供排查；恢复用 rollback 接口）——返回 200 是发布结果而非系统错误，
///   调用方按 `data.status` 分支，不要按 HTTP 5xx 重试。
/// - 幂等：目标 release 已 Active 时直接返回。
/// - `readiness_timeout_seconds` 默认 300、范围 5..=1800（Java 等慢启动应用可调大）。
///   **等待期间本请求同步阻塞**，调用方 HTTP 读超时须 ≥ 该值 + 余量。
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/releases/{release_id}/activate",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("release_id" = String, Path, description = "发布 ID（须先 prepare）")
    ),
    request_body = ActivateReleaseRequest,
    responses(
        (status = 200, description = "status=Active（就绪提交）或 Failed（就绪失败，现场保留，用 rollback 恢复）", body = HttpResult<ReleaseInfo>),
        (status = 400, description = "参数校验失败（含 readiness_timeout_seconds 越界 5..=1800）", body = HttpResult<String>),
        (status = 404, description = "应用不存在 / release 不存在 / 制品包文件缺失", body = HttpResult<String>),
        (status = 500, description = "切流/拉起/索引写入失败（操作错误，非就绪失败）", body = HttpResult<String>)
    ),
    tag = "应用发布"
)]
pub async fn activate_release(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, release_id)): Path<(String, String)>,
    Json(request): Json<ActivateReleaseRequest>,
) -> Result<Json<HttpResult<ReleaseInfo>>, AppError> {
    if let Some(seconds) = request.readiness_timeout_seconds
        && !(MIN_READY_TIMEOUT_SECS..=MAX_READY_TIMEOUT_SECS).contains(&seconds)
    {
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            format!(
                "readiness_timeout_seconds must be within {MIN_READY_TIMEOUT_SECS}..={MAX_READY_TIMEOUT_SECS}, got {seconds}"
            ),
        ));
    }
    Ok(Json(HttpResult::success(
        state
            .app_service
            .activate_release(&app_id, &release_id, request.readiness_timeout_seconds)
            .await?,
    )))
}

/// 回滚到最近一次成功版本（`.rollback` 快照恢复，秒级）
///
/// - **有快照**（最近一次 activate 失败、现场还在）：stop → 清失败版 code → 快照恢复 → start；
///   失败版 release 保持 Failed（message 记入 failure_message）。
/// - **无快照**（最近一次部署是成功的）：幂等返回当前 Active。
/// - **首次发布失败**（无旧版本可回滚）：409。
/// - 长期观察后的回退（`.rollback` 已被提交清理）：activate 旧 Prepared release_id（分钟级）。
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/releases/rollback",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body = RollbackReleaseRequest,
    responses(
        (status = 200, description = "恢复后的 Active release（无快照时幂等返回当前 Active）", body = HttpResult<ReleaseInfo>),
        (status = 400, description = "参数校验失败", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 409, description = "首次发布失败，无旧版本可回滚", body = HttpResult<String>),
        (status = 500, description = "停止/恢复/重启失败", body = HttpResult<String>)
    ),
    tag = "应用发布"
)]
pub async fn rollback_release(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<RollbackReleaseRequest>,
) -> Result<Json<HttpResult<ReleaseInfo>>, AppError> {
    Ok(Json(HttpResult::success(
        state
            .app_service
            .rollback_release(&app_id, request.message)
            .await?,
    )))
}

/// 列出应用全部 release（读 releases/index.json：active/最近失败指针 + 保留策略内列表）
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/releases",
    params(("app_id" = String, Path, description = "应用 ID")),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<ReleaseListResponse>),
        (status = 400, description = "参数校验失败", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 500, description = "索引读取失败", body = HttpResult<String>)
    ),
    tag = "应用发布"
)]
pub async fn list_releases(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<ReleaseListResponse>>, AppError> {
    Ok(Json(HttpResult::success(
        state.app_service.list_releases(&app_id).await?,
    )))
}

/// 删除 release 记录与制品包（保留策略外的手工清理）
///
/// active 的 release 不可删（409）；索引先行提交，包文件 best-effort 删除。
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/releases/{release_id}/delete",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("release_id" = String, Path, description = "发布 ID（非 active）")
    ),
    responses(
        (status = 200, description = "删除成功", body = HttpResult<String>),
        (status = 400, description = "参数校验失败", body = HttpResult<String>),
        (status = 404, description = "应用不存在 / release 不存在", body = HttpResult<String>),
        (status = 409, description = "active 状态的 release 不可删除", body = HttpResult<String>),
        (status = 500, description = "索引写入失败", body = HttpResult<String>)
    ),
    tag = "应用发布"
)]
pub async fn delete_release(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, release_id)): Path<(String, String)>,
) -> Result<Json<HttpResult<String>>, AppError> {
    state
        .app_service
        .delete_release(&app_id, &release_id)
        .await?;
    Ok(Json(HttpResult::success("删除成功".to_string())))
}
