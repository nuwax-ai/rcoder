//! 文件管理 handler（upload / list / delete）

use std::sync::Arc;

use axum::{
    Json,
    extract::{Multipart, Path, Query, State},
};
use serde::Deserialize;
use tracing::{info, instrument};
use utoipa::ToSchema;

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{FileInfo, UploadResult};

/// 上传文件
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/upload",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    request_body(content_type = "multipart/form-data", description = "上传文件"),
    responses(
        (status = 200, description = "上传成功", body = HttpResult<UploadResult>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, multipart))]
pub async fn upload_file(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    mut multipart: Multipart,
) -> Result<Json<HttpResult<UploadResult>>, AppError> {
    info!("[APP] uploading file: {}", app_id);

    let mut file_data: Option<Vec<u8>> = None;
    let mut file_name: Option<String> = None;
    let mut target_path: Option<String> = None;
    let mut flatten = false; // 压缩包上传：是否剥单层 wrapper 目录（默认 false 保留结构）

    // 解析 multipart 数据
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::bad_request(&format!("failed to parse upload: {}", e)))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                file_name = field.file_name().map(|s| s.to_string());
                let data = field.bytes().await.map_err(|e| {
                    AppError::bad_request(&format!("failed to read file data: {}", e))
                })?;
                file_data = Some(data.to_vec());
            }
            "target" => {
                let data = field.text().await.map_err(|e| {
                    AppError::bad_request(&format!("failed to read target path: {}", e))
                })?;
                target_path = Some(data);
            }
            "flatten" => {
                let data = field.text().await.map_err(|e| {
                    AppError::bad_request(&format!("failed to read flatten: {}", e))
                })?;
                flatten = data == "true" || data == "1";
            }
            _ => {}
        }
    }

    // 验证必需字段
    let data = file_data.ok_or_else(|| AppError::bad_request("missing file field"))?;
    let name = file_name.unwrap_or_else(|| "uploaded_file".to_string());
    let target = target_path.unwrap_or_else(|| format!("code/{}", name));

    let result = state
        .app_service
        .upload_file(&app_id, data, &target, flatten)
        .await?;

    Ok(Json(HttpResult::success(result)))
}

/// 从 URL 下载文件请求
#[derive(Debug, Deserialize, ToSchema)]
pub struct UploadFromUrlRequest {
    /// 下载 URL（http/https；SSRF 防护：默认拒私网/保留地址）
    pub url: String,
    /// 目标路径（app 根相对；单文件=文件路径，压缩包=解压目录如 "code/"；默认 "code/"）
    pub target: Option<String>,
    /// 压缩包是否剥单层 wrapper 目录（默认 false）
    pub flatten: Option<bool>,
}

/// 从 URL 下载文件/压缩包并上传
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/upload-from-url",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body = UploadFromUrlRequest,
    responses(
        (status = 200, description = "下载并上传成功", body = HttpResult<UploadResult>),
        (status = 400, description = "URL 非法 / SSRF 拒绝", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn upload_from_url(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(req): Json<UploadFromUrlRequest>,
) -> Result<Json<HttpResult<UploadResult>>, AppError> {
    info!("[APP] upload from url: {} (url={})", app_id, req.url);
    let target = req.target.unwrap_or_else(|| "code/".to_string());
    let flatten = req.flatten.unwrap_or(false);
    let result = state
        .app_service
        .upload_from_url(&app_id, &req.url, &target, flatten)
        .await?;
    Ok(Json(HttpResult::success(result)))
}

/// 列出文件查询参数
#[derive(Debug, Deserialize, Default, ToSchema)]
pub struct ListFilesQuery {
    /// 子目录（相对 app 根，如 "code"/"data"/"logs"；默认列 app 根）
    pub path: Option<String>,
}

/// 列出文件
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/files",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("path" = Option<String>, Query, description = "子目录（相对 app 根，如 code/data/logs；默认列 app 根）")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<Vec<FileInfo>>),
        (status = 404, description = "应用/路径不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn list_files(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Query(q): Query<ListFilesQuery>,
) -> Result<Json<HttpResult<Vec<FileInfo>>>, AppError> {
    info!("[APP] listing files: {} (subpath={:?})", app_id, q.path);
    let files = state
        .app_service
        .list_files(&app_id, q.path.as_deref())
        .await?;
    Ok(Json(HttpResult::success(files)))
}

/// 删除文件请求
#[derive(Debug, Deserialize, ToSchema)]
pub struct DeleteFileRequest {
    /// 文件路径（app 根相对，如 "code/app.jar"，可指向 code/data/logs 下任意文件）
    pub path: String,
}

/// 删除文件
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/files/delete",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    request_body = DeleteFileRequest,
    responses(
        (status = 200, description = "删除成功", body = HttpResult<String>),
        (status = 404, description = "文件/应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
pub async fn delete_file(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<DeleteFileRequest>,
) -> Result<Json<HttpResult<String>>, AppError> {
    info!("[APP] deleting file: {}/{}", app_id, request.path);
    state
        .app_service
        .delete_file(&app_id, &request.path)
        .await?;
    Ok(Json(HttpResult::success("文件删除成功".to_string())))
}
