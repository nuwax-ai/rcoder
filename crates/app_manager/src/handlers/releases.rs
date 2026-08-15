//! 应用发布 handler（prepare / activate / confirm / abort / list / delete）
//!
//! 发布状态机：prepare（下载校验入库）→ activate（切流，置 pending）→
//! confirm（健康确认转 Active；不健康自动回滚）→ 终态。abort 为 pending 残留的运维自救。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use shared_types::{AppError, HttpResult};

use crate::models::{
    AbortReleaseRequest, ActivateReleaseRequest, ConfirmReleaseRequest, PrepareReleaseRequest,
    ReleaseInfo, ReleaseListResponse,
};

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
        (status = 200, description = "预备成功（或幂等命中既有记录），status=Prepared", body = ReleaseInfo),
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
) -> Result<Json<ReleaseInfo>, AppError> {
    Ok(Json(
        state.app_service.prepare_release(&app_id, request).await?,
    ))
}

/// 激活发布：切换 code 软链并拉起应用，release 置 pending 等待健康确认
///
/// 幂等：目标 release 已 Active 或 pending 恢复（中断重启）时直接返回。
/// 上一个 release 仍处 pending（未 confirm）时拒绝（409）。
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/releases/{release_id}/activate",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("release_id" = String, Path, description = "发布 ID（须先 prepare）")
    ),
    request_body = ActivateReleaseRequest,
    responses(
        (status = 200, description = "激活流程完成，status=PendingStart，等待 confirm 健康确认", body = ReleaseInfo),
        (status = 400, description = "参数校验失败", body = HttpResult<String>),
        (status = 404, description = "应用不存在 / release 不存在 / 制品包文件缺失", body = HttpResult<String>),
        (status = 409, description = "另一 release 仍处于待确认（pending）状态", body = HttpResult<String>),
        (status = 500, description = "切流/拉起/索引写入失败", body = HttpResult<String>)
    ),
    tag = "应用发布"
)]
pub async fn activate_release(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, release_id)): Path<(String, String)>,
    Json(_request): Json<ActivateReleaseRequest>,
) -> Result<Json<ReleaseInfo>, AppError> {
    Ok(Json(
        state
            .app_service
            .activate_release(&app_id, &release_id)
            .await?,
    ))
}

/// 确认发布健康结果：healthy=true 提交（转 Active）；false 自动回滚到上一 Active
///
/// 幂等：重复 confirm 已终态（Active/Failed）且结论一致时直接返回；
/// 非待确认状态返回 409。首次发布 unhealthy 无可回滚版本时按失败清理。
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/releases/{release_id}/confirm",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("release_id" = String, Path, description = "发布 ID（须处于 pending 状态）")
    ),
    request_body = ConfirmReleaseRequest,
    responses(
        (status = 200, description = "确认完成：healthy=true 转 Active；false 回滚后置 Failed", body = ReleaseInfo),
        (status = 400, description = "参数校验失败", body = HttpResult<String>),
        (status = 404, description = "应用不存在 / release 不存在", body = HttpResult<String>),
        (status = 409, description = "release 非待确认状态（未 activate 或已终态且结论不一致）", body = HttpResult<String>),
        (status = 500, description = "回滚前置停止或索引写入失败", body = HttpResult<String>)
    ),
    tag = "应用发布"
)]
pub async fn confirm_release(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, release_id)): Path<(String, String)>,
    Json(request): Json<ConfirmReleaseRequest>,
) -> Result<Json<ReleaseInfo>, AppError> {
    Ok(Json(
        state
            .app_service
            .confirm_release(&app_id, &release_id, request.healthy, request.message)
            .await?,
    ))
}

/// 中止 pending 发布（运维自救：仅清 index，不动文件/运行时）
///
/// 针对 confirm(healthy=false) 自身失败导致 pending_release_id 永久残留的死局。
/// CAS 语义：仅当 index pending 恰指向该 release 时置 Failed + 清 pending；
/// 已 Failed 视为已中止（幂等返回）。
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/releases/{release_id}/abort",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("release_id" = String, Path, description = "发布 ID（须处于 pending 状态）")
    ),
    request_body = AbortReleaseRequest,
    responses(
        (status = 200, description = "中止成功（或幂等命中已 Failed），status=Failed", body = ReleaseInfo),
        (status = 400, description = "参数校验失败", body = HttpResult<String>),
        (status = 404, description = "应用不存在 / release 不存在", body = HttpResult<String>),
        (status = 409, description = "release 非 pending 且非已 Failed（如已 Active）", body = HttpResult<String>),
        (status = 500, description = "索引写入失败", body = HttpResult<String>)
    ),
    tag = "应用发布"
)]
pub async fn abort_release(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, release_id)): Path<(String, String)>,
    Json(request): Json<AbortReleaseRequest>,
) -> Result<Json<ReleaseInfo>, AppError> {
    Ok(Json(
        state
            .app_service
            .abort_release(&app_id, &release_id, request.message)
            .await?,
    ))
}

/// 列出应用全部 release（读 releases/index.json：active/pending 指针 + 保留策略内列表）
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/releases",
    params(("app_id" = String, Path, description = "应用 ID")),
    responses(
        (status = 200, description = "查询成功", body = ReleaseListResponse),
        (status = 400, description = "参数校验失败", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 500, description = "索引读取失败", body = HttpResult<String>)
    ),
    tag = "应用发布"
)]
pub async fn list_releases(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<ReleaseListResponse>, AppError> {
    Ok(Json(state.app_service.list_releases(&app_id).await?))
}

/// 删除 release 记录与制品包（保留策略外的手工清理）
///
/// active / pending 的 release 不可删（409）；索引先行提交，包文件 best-effort 删除。
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/releases/{release_id}/delete",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("release_id" = String, Path, description = "发布 ID（非 active/pending）")
    ),
    responses(
        (status = 200, description = "删除成功", body = serde_json::Value),
        (status = 400, description = "参数校验失败", body = HttpResult<String>),
        (status = 404, description = "应用不存在 / release 不存在", body = HttpResult<String>),
        (status = 409, description = "active/pending 状态的 release 不可删除", body = HttpResult<String>),
        (status = 500, description = "索引写入失败", body = HttpResult<String>)
    ),
    tag = "应用发布"
)]
pub async fn delete_release(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, release_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    state
        .app_service
        .delete_release(&app_id, &release_id)
        .await?;
    Ok(Json(serde_json::json!({"success": true})))
}
