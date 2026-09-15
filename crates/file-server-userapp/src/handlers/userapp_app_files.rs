//! app_manager 文件面转发目标的容器侧实现（生产运行容器 / 开发容器均可挂载）。
//!
//! rcoder 的 `/api/v1/userapp/{id}/upload|upload-from-url|files|files/delete` 四接口在
//! RBD 卷形态下不再直读写卷，改为**唤醒 + 转发到本容器 60000**；本模块按该四接口
//! 的原语义（魔数识别 zip/tar.gz → 解压 + flatten、app 根相对路径、防穿越）在
//! [`resolve_userapp_dev`] 根上原地实现——生产运行容器 = 单 app 模式（卷根即 app 根），
//! 开发容器 = `{ws}/{app_id}`（对称可用，虽然 app_manager 只对生产容器转发）。
//!
//! 与 userapp_files.rs（Java 15 镜像族）的区别：本族是 rcoder↔file-server 的内部
//! 契约（字段直传、响应形状对齐 app_manager DTO），不经 Java。请求键 snake_case
//! 单键——本族为 userApp 专属新契约，未上线不做旧 camel 键兼容。

use axum::Json;
use axum::extract::{Multipart, Query, State};
use serde_json::json;
use tracing::info;

use crate::UserAppState;
use crate::handlers::userapp_files::require_app_field;
use file_server::error::{AppError, AppResult};
use file_server::ops::multipart::{file_field, text_field};
use file_server::workspace::resolve_userapp_dev;

use download_utils::{
    DownloadConfig, Downloader, detect_file_type_from_path, extract_tar_gz, extract_zip,
    normalize_extracted_dir,
};
use tokio_util::sync::CancellationToken;

use crate::models::{
    AppFilesClearBody, AppFilesDeleteBody, AppFilesListParams, AppFilesUploadForm,
    AppFilesUploadFromUrlBody,
};

// ── 上传（multipart: app_id / target / flatten / file）──────────────────────────

/// 上传文件
///
/// zip/tar.gz 自动解压；单文件直写。
#[utoipa::path(post, path = "/app-files/upload", request_body(content = AppFilesUploadForm, content_type = "multipart/form-data"), responses(file_server::openapi::JsonApiResponses), tag = "Userapp · 双态 · 文件与存储")]
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
            "app_id" => app_id = Some(text_field(field).await?),
            "user_id" => user_id = Some(text_field(field).await?),
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
    let app_id = require_app_field(app_id, "app_id")?;
    let user_id = require_app_field(user_id, "user_id")?;
    let target = require_app_field(target, "target")?;
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;
    let _workspace_activity = state
        .build_tasks
        .workspace_activity(&app_id)
        .await
        .read_owned()
        .await;
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
            ensure_within_root(&dest, root).await?;
            let count = tokio::task::spawn_blocking({
                let dest = dest.clone();
                let archive = archive_path.to_path_buf();
                // 配额传 None: 保持既有行为（容器/PVC 配额作主边界，
                // 见 download_utils::archive 模块文档）
                move || match file_type {
                    "zip" => extract_zip(&archive, &dest, None),
                    _ => extract_tar_gz(&archive, &dest, None),
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
                ensure_within_root(parent, root).await?;
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

/// 容器内流式下载后走上传核心
#[utoipa::path(
    post,
    path = "/app-files/upload-from-url",
    request_body = AppFilesUploadFromUrlBody,
    description = r#"
服务端代下载后落盘（制品库/对象存储直连发布场景，免本地中转）：HTTP(S) 下载
→ 压缩包按魔数自动解压（zip/tar.gz）、单文件直写。语义同 `upload`：
`target` 为 app 根相对落盘位置、`flatten` 控制剥单层 wrapper 目录。
"#,
    responses(file_server::openapi::JsonApiResponses),
    tag = "Userapp · 双态 · 文件与存储",
)]
pub(crate) async fn upload_from_url(
    State(state): State<UserAppState>,
    Json(body): Json<AppFilesUploadFromUrlBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let _workspace_activity = state
        .build_tasks
        .workspace_activity(&body.app_id)
        .await
        .read_owned()
        .await;
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
    info!(app_id = %body.app_id, url = %body.url, "app-files upload-from-url done");
    Ok(Json(json!({
        "success": true,
        "file_path": result.file_path,
        "file_size": result.file_size,
        "uploaded_at": result.uploaded_at,
        "extracted_count": result.extracted_count,
    })))
}

// ── 列表（GET ?path=）───────────────────────────────────────────────────────────

/// 列目录（app 根相对 path 字段）
#[utoipa::path(
    get,
    path = "/app-files/list",
    params(AppFilesListParams),
    description = r#"
列应用卷内指定目录的文件清单（名称/大小/mtime 元信息）。响应形状对齐
rcoder app_manager DTO（本族为 rcoder↔file-server 内部契约，字段直传不经
Java）；生产运行容器 = 单 app 模式（卷根即 app 根）。
"#,
    responses(file_server::openapi::JsonApiResponses),
    tag = "Userapp · 双态 · 文件与存储"
)]
pub(crate) async fn list(
    State(state): State<UserAppState>,
    Query(params): Query<AppFilesListParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    tracing::debug!(app_id = %params.app_id, "app-files list");
    let _workspace_activity = state
        .build_tasks
        .workspace_activity(&params.app_id)
        .await
        .read_owned()
        .await;
    let root = resolve_userapp_dev(&params.app_id, None, &state.fs.config)?;
    if !tokio::fs::try_exists(&root).await.unwrap_or(false) {
        return Ok(Json(json!({"success": true, "files": []})));
    }
    let canonical_root = tokio::fs::canonicalize(&root)
        .await
        .map_err(|e| AppError::system(format!("resolve app root: {e}")))?;
    let sub = params
        .path
        .as_deref()
        .map(|p| p.trim_end_matches('/'))
        .filter(|p| !p.is_empty());
    let target_dir = match sub {
        Some(p) => {
            let full = root.join(p);
            if !tokio::fs::try_exists(&full).await.unwrap_or(false) {
                return Ok(Json(json!({"success": true, "files": []})));
            }
            ensure_within_root(&full, &canonical_root).await?
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

/// 删除文件或目录（防穿越）
#[utoipa::path(
    post,
    path = "/app-files/delete",
    request_body = AppFilesDeleteBody,
    description = r#"
按路径删除文件或目录（app 根相对；路径解析经防穿越校验，拒绝越出卷根的
`..` 与绝对路径注入）。危险的全量清理不走此接口——由存储面 storage/clear 承担。
"#,
    responses(file_server::openapi::JsonApiResponses),
    tag = "Userapp · 双态 · 文件与存储",
)]
pub(crate) async fn delete(
    State(state): State<UserAppState>,
    Json(body): Json<AppFilesDeleteBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    info!(app_id = %body.app_id, path = %body.path, "app-files delete");
    let _workspace_activity = state
        .build_tasks
        .workspace_activity(&body.app_id)
        .await
        .read_owned()
        .await;
    let root = resolve_userapp_dev(&body.app_id, None, &state.fs.config)?;
    if !tokio::fs::try_exists(&root).await.unwrap_or(false) {
        return Err(AppError::resource(format!(
            "app root does not exist: {}",
            root.display()
        )));
    }
    let full = root.join(&body.path);
    if !tokio::fs::try_exists(&full).await.unwrap_or(false) {
        return Err(AppError::resource(format!(
            "file does not exist: {}",
            body.path
        )));
    }
    let canonical_root = tokio::fs::canonicalize(&root)
        .await
        .map_err(|e| AppError::system(format!("resolve app root: {e}")))?;
    let canonical = ensure_within_root(&full, &canonical_root).await?;
    if tokio::fs::metadata(&canonical)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
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

// ── 清空 workspace（json {app_id}）─────────────────────────────────────────────

static CLEAR_INSTANCE: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| uuid::Uuid::new_v4().to_string());

/// 查询开发工作区重置实例
///
/// 内部控制协议：在捕获的单个 builder 实例上读取进程身份，不通过负载均衡地址。
/// 调用方随后将返回的 instance_id 作为 expected_instance_id 提交至 clear；
/// 查询不创建工作区、不清理文件。成功返回未包装的目标 DTO，业务失败沿用错误信封。
#[utoipa::path(get, path = "/app-files/clear-target",
    params(
        ("app_id"=String, Query, description = "Application identifier whose workspace will be reset"),
        ("user_id"=String, Query, description = "Application owner identifier from the lifecycle operation")
    ),
    responses((status=200, description="Reset target identity, or a business error envelope", body=shared_types::UserAppWorkspaceClearTarget)),
    tag = "Userapp · 双态 · 文件与存储")]
pub(crate) async fn clear_target(
    State(state): State<UserAppState>,
    Query(query): Query<shared_types::UserAppWorkspaceClearProbe>,
) -> Result<Json<shared_types::UserAppWorkspaceClearTarget>, AppError> {
    resolve_userapp_dev(&query.app_id, None, &state.fs.config)?;
    Ok(Json(shared_types::UserAppWorkspaceClearTarget {
        app_id: query.app_id,
        instance_id: CLEAR_INSTANCE.clone(),
    }))
}

fn validate_clear_instance(body: &AppFilesClearBody, current: &str) -> AppResult<()> {
    shared_types::validate_identifier(&body.app_id, "app_id").map_err(AppError::validation)?;
    if body.expected_instance_id.is_empty() || body.expected_instance_id != current {
        return Err(AppError::business(
            "Workspace clear target changed; observe its current identity before submitting a new operation",
        ));
    }
    Ok(())
}

/// 清空 workspace 内容（留容器留卷）
///
/// rcoder `POST /api/v1/userapp/{app_id}/dev/storage/clear` 的容器侧实现：
/// "重置开发工作区"语义——逐子项删除 `resolve_userapp_dev` 根下全部内容、保留
/// 根目录本身；幂等（workspace 不存在视为已空）。
#[utoipa::path(
    post,
    path = "/app-files/clear",
    request_body = AppFilesClearBody,
    description = r#"
清空 workspace 内容、**留容器留卷**（"重置开发工作区"）：逐子项删除根下全部
内容、保留根目录本身。幂等：workspace 不存在视为已空直接成功。与 prod 的
storage/clear（K8s 删 PVC 重建空卷）语义不同——开发容器常驻，卷重建要求先
销毁容器，得不偿失。

内部控制请求须携带 app_id、user_id 及通过 clear-target 捕获的 expected_instance_id。
进程实例已替换时拒绝请求，不清理替代实例。成功响应为未包装的
{success: true, instance_id}，调用方必须核对 instance_id；HTTP 200 错误信封不代表清理成功。
"#,
    responses((status=200, description="Confirmed reset acknowledgement, or a business error envelope", body=shared_types::UserAppWorkspaceClearResult)),
    tag = "Userapp · 双态 · 文件与存储"
)]
pub(crate) async fn clear(
    State(state): State<UserAppState>,
    Json(body): Json<AppFilesClearBody>,
) -> Result<Json<shared_types::UserAppWorkspaceClearResult>, AppError> {
    validate_clear_instance(&body, &CLEAR_INSTANCE)?;
    info!(app_id = %body.app_id, "app-files clear (workspace reset)");
    let root = resolve_userapp_dev(&body.app_id, None, &state.fs.config)?;
    // Dropping the HTTP observer must not release a lease while filesystem
    // operations accepted by the worker are still running.
    tokio::spawn(reset_workspace(state, body.app_id, root))
        .await
        .map_err(|error| {
            AppError::system(format!("Workspace reset worker interrupted: {error}"))
        })??;
    Ok(Json(shared_types::UserAppWorkspaceClearResult {
        success: true,
        instance_id: CLEAR_INSTANCE.clone(),
    }))
}

async fn reset_workspace(
    state: UserAppState,
    app_id: String,
    root: std::path::PathBuf,
) -> AppResult<()> {
    let directory = shared_types::storage_contents::StorageDirectoryLease::capture(&root)
        .await
        .map_err(|error| AppError::system(format!("Capture workspace directory: {error}")))?;
    let lifecycle = state.build_tasks.dev_lifecycle(&app_id).await;
    {
        let mut generation = lifecycle.lock().await;
        *generation = generation
            .checked_add(1)
            .ok_or_else(|| AppError::system("Dev lifecycle generation exhausted"))?;
        for task in state.build_tasks.active_tasks_for_app(&app_id).await {
            super::userapp::cancel_build_task(&task).await;
        }
    }
    // Release lifecycle before awaiting workers: commit_start needs it in order
    // to observe cancellation and release its workspace activity lease.
    let activity = state.build_tasks.workspace_activity(&app_id).await;
    let _exclusive =
        tokio::time::timeout(std::time::Duration::from_secs(90), activity.write_owned())
            .await
            .map_err(|_| {
                AppError::business("Workspace workers did not finish before reset deadline")
            })?;
    let mut generation = lifecycle.lock().await;
    directory.validate_current().await.map_err(|error| {
        AppError::business(format!("Workspace directory changed before reset: {error}"))
    })?;
    *generation = generation
        .checked_add(1)
        .ok_or_else(|| AppError::system("Dev lifecycle generation exhausted"))?;
    // Include newer tasks that finished before the exclusive lease was queued.
    let key = super::userapp_dev_server::dev_key(&app_id);
    let stopped = state.fs.dev_server.stop_dev(&key).await?;
    if stopped.killed_pids.iter().any(|process| !process.killed) {
        return Err(AppError::business(
            "Development processes are still running; workspace was not cleared",
        ));
    }
    state.fs.log_cache.delete(&key)?;
    directory.clear().await.map_err(|error| {
        AppError::system(format!("Clear workspace {}: {error}", root.display()))
    })?;
    info!(%app_id, "app-files clear done (root retained)");
    Ok(())
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
async fn ensure_within_root(
    path: &std::path::Path,
    canonical_root: &std::path::Path,
) -> AppResult<std::path::PathBuf> {
    let canonical = tokio::fs::canonicalize(path)
        .await
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

#[cfg(test)]
mod clear_identity_tests {
    use super::*;

    #[tokio::test]
    async fn registered_routes_preserve_the_internal_reset_wire_contract() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt as _;

        let directory = tempfile::tempdir().expect("fixture");
        let state = super::super::userapp_files::tests_support::make_state(directory.path().into());
        let workspace = directory.path().join("clear-wire");
        tokio::fs::create_dir_all(&workspace)
            .await
            .expect("workspace");
        tokio::fs::write(workspace.join("keep.txt"), b"original")
            .await
            .expect("content");
        let (router, _) = crate::routes::userapp_top_router().split_for_parts();
        let router = router.with_state(state);
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            router.clone().oneshot(
                Request::builder()
                    .uri("/api/v1/userapp/app-files/clear-target?app_id=clear-wire&user_id=owner")
                    .body(Body::empty())
                    .expect("probe"),
            ),
        )
        .await
        .expect("probe deadline")
        .expect("probe response");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("probe body");
        let target: shared_types::UserAppWorkspaceClearTarget =
            serde_json::from_slice(&bytes).expect("unwrapped target DTO");
        assert_eq!(target.app_id, "clear-wire");
        assert!(!target.instance_id.is_empty());

        for (instance, succeeds) in [
            ("previous-instance", false),
            (target.instance_id.as_str(), true),
        ] {
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                router.clone().oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/userapp/app-files/clear")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::to_vec(&AppFilesClearBody {
                                app_id: target.app_id.clone(),
                                expected_instance_id: instance.into(),
                            })
                            .expect("clear JSON"),
                        ))
                        .expect("clear request"),
                ),
            )
            .await
            .expect("clear deadline")
            .expect("clear response");
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .expect("clear body");
            let result =
                serde_json::from_slice::<shared_types::UserAppWorkspaceClearResult>(&bytes);
            if succeeds {
                assert_eq!(status, axum::http::StatusCode::OK);
                assert!(
                    result
                        .expect("unwrapped reset DTO")
                        .confirms(&target.instance_id)
                );
                assert!(!workspace.join("keep.txt").exists());
                assert!(workspace.is_dir());
            } else {
                assert!(!status.is_success());
                assert!(
                    result.is_err(),
                    "business error must not decode as a reset acknowledgement"
                );
                assert_eq!(
                    tokio::fs::read(workspace.join("keep.txt"))
                        .await
                        .expect("retained content"),
                    b"original"
                );
            }
        }
    }

    #[tokio::test]
    async fn replaced_instance_is_rejected_before_workspace_mutation() {
        let directory = tempfile::tempdir().expect("fixture");
        let state = super::super::userapp_files::tests_support::make_state(directory.path().into());
        let workspace = directory.path().join("clear-identity");
        tokio::fs::create_dir_all(&workspace)
            .await
            .expect("workspace");
        tokio::fs::write(workspace.join("keep.txt"), b"original")
            .await
            .expect("content");
        let target = clear_target(
            State(state.clone()),
            Query(shared_types::UserAppWorkspaceClearProbe {
                app_id: "clear-identity".into(),
            }),
        )
        .await
        .expect("target identity")
        .0;
        assert!(!target.instance_id.is_empty());
        let result = clear(
            State(state.clone()),
            Json(AppFilesClearBody {
                app_id: "clear-identity".into(),
                expected_instance_id: "previous-process".into(),
            }),
        )
        .await;
        assert!(matches!(result, Err(AppError::Business(_))));
        assert_eq!(
            tokio::fs::read(workspace.join("keep.txt"))
                .await
                .expect("retained content"),
            b"original"
        );
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            clear(
                State(state),
                Json(AppFilesClearBody {
                    app_id: "clear-identity".into(),
                    expected_instance_id: target.instance_id.clone(),
                }),
            ),
        )
        .await
        .expect("clear deadline")
        .expect("matching instance");
        assert!(result.0.success);
        assert_eq!(result.0.instance_id, target.instance_id);
        assert!(workspace.is_dir());
        assert!(
            tokio::fs::read_dir(workspace)
                .await
                .expect("root")
                .next_entry()
                .await
                .expect("entry")
                .is_none()
        );
    }

    #[test]
    fn clear_requires_an_explicit_instance_identity_on_the_wire() {
        assert!(
            serde_json::from_value::<AppFilesClearBody>(json!({"app_id":"app", "user_id":"owner"}))
                .is_err()
        );
        let request = AppFilesClearBody {
            app_id: "app".into(),
            expected_instance_id: String::new(),
        };
        assert!(validate_clear_instance(&request, "current").is_err());
    }
}
