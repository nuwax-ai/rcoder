//! app_manager 文件面转发目标的容器侧实现（生产运行容器 / 开发容器均可挂载）。
//!
//! rcoder 的 `/api/v1/userapp/{id}/upload|upload-from-url|files|files/delete` 四接口在
//! RBD 卷形态下不再直读写卷，改为**唤醒 + 转发到本容器 60000**；本模块按该四接口
//! 的原语义（魔数识别 zip/tar.gz → 解压 + flatten、app 根相对路径、防穿越）在
//! [`resolve_userapp_dev`] 根上原地实现——生产运行容器 = 单 app 模式（卷根即 app 根），
//! 开发容器 = `{ws}/{app_id}`（对称可用，虽然 app_manager 只对生产容器转发）。
//!
//! 与 userapp_files.rs（Java 15 镜像族）的区别：本族是 rcoder↔file-server 的内部
//! 契约（字段直传、响应形状对齐 app_manager DTO），不经 Java。

use axum::Json;
use axum::extract::{Multipart, Query, State};
use serde::Deserialize;
use serde_json::json;
use tracing::info;

use crate::UserAppState;
use crate::handlers::userapp_files::require_app_field;
use file_server::error::{AppError, AppResult};
use file_server::handlers::multipart::{file_field, text_field};
use file_server::workspace::resolve_userapp_dev;

use download_utils::{
    DownloadConfig, Downloader, detect_file_type_from_path, extract_tar_gz, extract_zip,
    normalize_extracted_dir,
};
use tokio_util::sync::CancellationToken;

// ── 上传（multipart: appId / target / flatten / file）────────────────────────────

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppFilesUploadForm {
    /// UserApp 应用 ID（定位 = resolve_userapp_dev；单 app 模式须与归属一致）
    pub app_id: String,
    /// 用户 ID（挂载压平契约字段：rcoder ensure builder 组装宿主树用；file-server
    /// 侧仅日志审计，不参与容器内定位）
    pub user_id: String,
    /// app 根相对目标（压缩包=解压目录；单文件=文件路径）
    pub target: String,
    /// 压缩包解压后单层归一（默认 false）
    pub flatten: Option<bool>,
    #[schema(format = Binary)]
    /// 上传内容（zip / tar.gz / 单文件，魔数自动识别）
    pub file: String,
}

/// 上传文件
///
/// zip/tar.gz 自动解压；单文件直写。
#[utoipa::path(post, path = "/app-files/upload", request_body(content = AppFilesUploadForm, content_type = "multipart/form-data"), responses(file_server::openapi::JsonApiResponses), tag = "UserApp · 开发与构建")]
pub(crate) async fn upload(
    State(state): State<UserAppState>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut app_id = None;
    let mut user_id = None;
    let mut target = None;
    let mut flatten = false;
    let mut data = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "appId" => app_id = Some(text_field(field).await?),
            "userId" => user_id = Some(text_field(field).await?),
            "target" => target = Some(text_field(field).await?),
            "flatten" => flatten = matches!(text_field(field).await?.trim(), "true" | "1" | "yes"),
            "file" => {
                data = Some(
                    file_field(
                        field,
                        state.fs.config.upload_max_file_size_bytes,
                        &state.fs.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                )
            }
            _ => {}
        }
    }
    let app_id = require_app_field(app_id, "appId")?;
    let user_id = require_app_field(user_id, "userId")?;
    let target = require_app_field(target, "target")?;
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;
    let root = resolve_userapp_dev(&app_id, None, &state.fs.config)?;
    let result = upload_impl(&root, &target, flatten, data.path(), data.size()).await?;
    info!(app_id = %app_id, user_id = %user_id, target = %target, "app-files upload done");
    Ok(Json(json!({
        "success": true,
        "file_path": result.file_path,
        "file_size": result.file_size,
        "uploaded_at": result.uploaded_at,
        "extracted_count": result.extracted_count,
    })))
}

struct UploadOutcome {
    file_path: String,
    file_size: u64,
    uploaded_at: String,
    extracted_count: Option<usize>,
}

/// 上传核心：魔数识别 → 压缩包解压（zip-slip 由 download_utils 防护）/ 单文件直写。
/// `archive_path` 已落盘（multipart file_field 下载到 temp），避免整包进内存。
async fn upload_impl(
    root: &std::path::Path,
    target: &str,
    flatten: bool,
    archive_path: &std::path::Path,
    file_size: u64,
) -> AppResult<UploadOutcome> {
    validate_target(target)?;
    let uploaded_at = chrono::Utc::now().to_rfc3339();
    let file_type = detect_file_type_from_path(archive_path)
        .map_err(|e| AppError::validation(format!("detect archive type: {e}")))?;
    match file_type {
        "zip" | "tar.gz" => {
            let dest = root.join(target.trim_end_matches('/'));
            tokio::fs::create_dir_all(&dest)
                .await
                .map_err(|e| AppError::system(format!("create extraction dir: {e}")))?;
            ensure_within_root(&dest, root)?;
            let count = tokio::task::spawn_blocking({
                let dest = dest.clone();
                let archive = archive_path.to_path_buf();
                move || match file_type {
                    "zip" => extract_zip(&archive, &dest),
                    _ => extract_tar_gz(&archive, &dest),
                }
            })
            .await
            .map_err(|e| AppError::system(format!("extraction task: {e}")))?
            .map_err(map_archive_error)?;
            if flatten {
                let dest_for_flatten = dest.clone();
                tokio::task::spawn_blocking(move || normalize_extracted_dir(&dest_for_flatten))
                    .await
                    .map_err(|e| AppError::system(format!("flatten task: {e}")))?
                    .map_err(map_archive_error)?;
            }
            Ok(UploadOutcome {
                file_path: target.to_string(),
                file_size,
                uploaded_at,
                extracted_count: Some(count),
            })
        }
        _ => {
            // 单文件：target = 文件路径（app 根相对）
            let file_path = root.join(target);
            if let Some(parent) = file_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| AppError::system(format!("create parent dir: {e}")))?;
                ensure_within_root(parent, root)?;
            }
            tokio::fs::copy(archive_path, &file_path)
                .await
                .map_err(|e| AppError::system(format!("write file: {e}")))?;
            Ok(UploadOutcome {
                file_path: target.to_string(),
                file_size,
                uploaded_at,
                extracted_count: None,
            })
        }
    }
}

// ── upload-from-url（json）───────────────────────────────────────────────────────

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppFilesUploadFromUrlBody {
    /// 制品/文件下载地址（HTTP(S)）
    pub url: String,
    /// app 根相对目标
    pub target: String,
    /// 压缩包解压后单层归一（默认 false）
    #[serde(default)]
    pub flatten: bool,
    /// UserApp 应用 ID（定位）
    pub app_id: String,
    /// 用户 ID（挂载压平契约字段：rcoder ensure builder 组装宿主树用；file-server
    /// 侧仅日志审计，不参与容器内定位）
    pub user_id: String,
}

/// 容器内流式下载后走上传核心
#[utoipa::path(post, path = "/app-files/upload-from-url", request_body = AppFilesUploadFromUrlBody, responses(file_server::openapi::JsonApiResponses), tag = "UserApp · 开发与构建")]
pub(crate) async fn upload_from_url(
    State(state): State<UserAppState>,
    Json(body): Json<AppFilesUploadFromUrlBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let root = resolve_userapp_dev(&body.app_id, None, &state.fs.config)?;
    let downloader = Downloader::new(DownloadConfig::default());
    let cancel = CancellationToken::new();
    let tmp = tokio::task::spawn_blocking(tempfile::NamedTempFile::new)
        .await
        .map_err(|e| AppError::system(format!("tempfile task: {e}")))?
        .map_err(|e| AppError::system(format!("create tempfile: {e}")))?;
    downloader
        .download_to_file(&body.url, tmp.path(), None, &cancel)
        .await
        .map_err(|e| AppError::validation(format!("download {}: {e}", body.url)))?;
    let size = tmp
        .as_file()
        .metadata()
        .map(|m| m.len())
        .map_err(|e| AppError::system(format!("stat downloaded file: {e}")))?;
    let result = upload_impl(&root, &body.target, body.flatten, tmp.path(), size).await?;
    info!(app_id = %body.app_id, user_id = %body.user_id, url = %body.url, "app-files upload-from-url done");
    Ok(Json(json!({
        "success": true,
        "file_path": result.file_path,
        "file_size": result.file_size,
        "uploaded_at": result.uploaded_at,
        "extracted_count": result.extracted_count,
    })))
}

// ── 列表（GET ?path=）───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
pub struct AppFilesListParams {
    /// UserApp 应用 ID（定位）
    pub app_id: String,
    /// 用户 ID（挂载压平契约字段：rcoder ensure builder 组装宿主树用；file-server
    /// 侧仅审计，不参与容器内定位）
    pub user_id: String,
    /// app 根相对子目录（缺省列根）
    #[serde(default)]
    pub path: Option<String>,
}

/// 列目录（app 根相对 path 字段）
#[utoipa::path(
    get,
    path = "/app-files/list",
    params(AppFilesListParams),
    responses(file_server::openapi::JsonApiResponses),
    tag = "UserApp · 开发与构建"
)]
pub(crate) async fn list(
    State(state): State<UserAppState>,
    Query(params): Query<AppFilesListParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    tracing::debug!(app_id = %params.app_id, user_id = %params.user_id, "app-files list");
    let root = resolve_userapp_dev(&params.app_id, None, &state.fs.config)?;
    if !root.exists() {
        return Ok(Json(json!({"success": true, "files": []})));
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|e| AppError::system(format!("resolve app root: {e}")))?;
    let sub = params
        .path
        .as_deref()
        .map(|p| p.trim_end_matches('/'))
        .filter(|p| !p.is_empty());
    let target_dir = match sub {
        Some(p) => {
            let full = root.join(p);
            if !full.exists() {
                return Ok(Json(json!({"success": true, "files": []})));
            }
            ensure_within_root(&full, &canonical_root)?
        }
        None => canonical_root,
    };
    let rel_prefix = sub.map(|p| format!("{p}/")).unwrap_or_default();
    let mut files = Vec::new();
    let mut entries = tokio::fs::read_dir(&target_dir)
        .await
        .map_err(|e| AppError::system(format!("read dir: {e}")))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| AppError::system(format!("traverse dir: {e}")))?
    {
        let metadata = entry
            .metadata()
            .await
            .map_err(|e| AppError::system(format!("read metadata: {e}")))?;
        files.push(json!({
            "path": format!("{rel_prefix}{}", entry.file_name().to_string_lossy()),
            "size": metadata.len(),
            "is_dir": metadata.is_dir(),
            "modified_at": metadata
                .modified()
                .ok()
                .map(|t| {
                    let datetime: chrono::DateTime<chrono::Utc> = t.into();
                    datetime.to_rfc3339()
                })
                .unwrap_or_default(),
        }));
    }
    Ok(Json(json!({"success": true, "files": files})))
}

// ── 删除（json {path}）──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppFilesDeleteBody {
    /// UserApp 应用 ID（定位）
    pub app_id: String,
    /// 用户 ID（挂载压平契约字段：rcoder ensure builder 组装宿主树用；file-server
    /// 侧仅日志审计，不参与容器内定位）
    pub user_id: String,
    /// app 根相对文件/目录
    pub path: String,
}

/// 删除文件或目录（防穿越）
#[utoipa::path(post, path = "/app-files/delete", request_body = AppFilesDeleteBody, responses(file_server::openapi::JsonApiResponses), tag = "UserApp · 开发与构建")]
pub(crate) async fn delete(
    State(state): State<UserAppState>,
    Json(body): Json<AppFilesDeleteBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    info!(app_id = %body.app_id, user_id = %body.user_id, path = %body.path, "app-files delete");
    let root = resolve_userapp_dev(&body.app_id, None, &state.fs.config)?;
    if !root.exists() {
        return Err(AppError::resource(format!(
            "app root does not exist: {}",
            root.display()
        )));
    }
    let full = root.join(&body.path);
    if !full.exists() {
        return Err(AppError::resource(format!(
            "file does not exist: {}",
            body.path
        )));
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|e| AppError::system(format!("resolve app root: {e}")))?;
    let canonical = ensure_within_root(&full, &canonical_root)?;
    if canonical.is_dir() {
        tokio::fs::remove_dir_all(&canonical)
            .await
            .map_err(|e| AppError::system(format!("remove dir: {e}")))?;
    } else {
        tokio::fs::remove_file(&canonical)
            .await
            .map_err(|e| AppError::system(format!("remove file: {e}")))?;
    }
    info!(app_id = %body.app_id, path = %body.path, "app-files deleted");
    Ok(Json(json!({"success": true})))
}

// ── 共用防护 ─────────────────────────────────────────────────────────────────────

/// target 校验：拒绝绝对路径与 `..` 穿越段（对齐 app_manager validate_upload_target）。
fn validate_target(target: &str) -> AppResult<()> {
    if target.is_empty() || target.starts_with('/') {
        return Err(AppError::validation(format!(
            "target must be a non-empty relative path: '{target}'"
        )));
    }
    if std::path::Path::new(target)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(AppError::validation(format!(
            "target must not contain '..': '{target}'"
        )));
    }
    Ok(())
}

/// canonicalize 后必须仍在 root 内（防符号链接穿越）。
fn ensure_within_root(
    path: &std::path::Path,
    canonical_root: &std::path::Path,
) -> AppResult<std::path::PathBuf> {
    let canonical = path
        .canonicalize()
        .map_err(|e| AppError::system(format!("resolve {}: {e}", path.display())))?;
    if !canonical.starts_with(canonical_root) {
        return Err(AppError::validation(format!(
            "path escapes app root: {}",
            path.display()
        )));
    }
    Ok(canonical)
}

fn map_archive_error(e: download_utils::ArchiveError) -> AppError {
    AppError::validation(format!("archive error: {e}"))
}
