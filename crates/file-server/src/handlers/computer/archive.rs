//! computer 打包下载 handlers: zip-workspace / download-all-files + zip 响应辅助。

use axum::extract::State;
use axum::response::Response;
use garde::Validate;

use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::models::{UserCidQuery, ZipBody};

use crate::ops::{download_all_files_impl, zip_workspace_impl};

use super::{resolve_computer_target, ws_path};

/// workspace 打包下载
///
/// 对齐 nuwax zipWorkspace:
/// 无顶层前缀; 工作区不存在则报错; 文件名 `${userId}_${cId}.zip` + UTF-8 filename*。
/// 过滤: ZIP_WORKSPACE_EXCLUDE (强制) + 调用方 excludeDirs (补充) 合并, 对任意路径段匹配
/// (目录与文件同集合); 跳过符号链接; **无** dot-segment 过滤 (保留 .gitignore/.npmrc 等)。
#[utoipa::path(
    post,
    path = "/zip-workspace",
    request_body = ZipBody,
    responses(
        (status = 200, description = "Workspace ZIP archive", body = crate::models::BinaryFile, content_type = "application/zip"),
        crate::openapi::ErrorApiResponses
    ),
    tag = "Computer"
)]
pub(crate) async fn zip_workspace(
    State(state): State<AppState>,
    Json(body): Json<ZipBody>,
) -> Result<Response, AppError> {
    let src = ws_path(&state, &body.user_id, &body.c_id).await?;
    let filename = format!("{}_{}.zip", body.user_id, body.c_id);
    zip_workspace_impl(&state, src, body.exclude_dirs.unwrap_or_default(), filename).await
}

/// 下载工作区全部文件
///
/// 对齐 nuwax downloadAllFiles:
/// 顶层前缀 `${userId}_${cId}/` + 空 zip 兜底 + 100MB 大小上限 + UTF-8 filename* + customTargetDir。
#[utoipa::path(
    get,
    path = "/download-all-files",
    params(UserCidQuery),
    responses(
        (status = 200, description = "Workspace ZIP archive", body = crate::models::BinaryFile, content_type = "application/zip"),
        crate::openapi::ErrorApiResponses
    ),
    tag = "Computer"
)]
pub(crate) async fn download_all_files(
    State(state): State<AppState>,
    Query(q): Query<UserCidQuery>,
) -> Result<Response, AppError> {
    q.validate().map_err(crate::error::from_garde)?;
    let src = resolve_computer_target(&state, &q.user_id, &q.c_id, q.custom_target_dir.as_deref())
        .await?;
    let prefix = format!("{}_{}/", q.user_id, q.c_id);
    let filename = format!("{}_{}.zip", q.user_id, q.c_id);
    download_all_files_impl(&state, src, prefix, filename).await
}
