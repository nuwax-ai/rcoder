//! computer 文件**写类** handlers: delete-workspace / files-update / upload-file /
//! upload-files / generate-file / import-project。
//!
//! 读类 handler (get-file-list / resolve-file / search-files) 见 [`super::files_read`]。

use std::path::Path;

use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppMultipart as Multipart};
use crate::path_safety;
use crate::service::code as code_service;

use super::{file_field, resolve_computer_target, text_field, validate_zip_ext, ws_path};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeleteWorkspaceBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    c_id: String,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadFileForm {
    pub user_id: String,
    pub c_id: String,
    pub file_path: String,
    pub custom_target_dir: Option<String>,
    #[schema(format = Binary)]
    pub file: String,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadFilesForm {
    pub user_id: String,
    pub c_id: String,
    pub custom_target_dir: Option<String>,
    pub file_paths: Vec<String>,
    pub files: Vec<crate::openapi::BinaryFile>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportProjectForm {
    pub user_id: String,
    pub c_id: String,
    pub custom_target_dir: Option<String>,
    #[schema(format = Binary)]
    pub file: String,
}

// ── delete-workspace ────────────────────────────────────────────────────────────

/// `POST /api/computer/delete-workspace` (对齐 nuwax deleteWorkspace; 目录不存在也返回 deleted)。
#[utoipa::path(post, path = "/delete-workspace", request_body = DeleteWorkspaceBody, responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn delete_workspace(
    State(state): State<AppState>,
    Json(body): Json<DeleteWorkspaceBody>,
) -> Result<Json<Value>, AppError> {
    let path = ws_path(&state, &body.user_id, &body.c_id).await?;
    // 不存在视为已删除 (对齐 nuwax, 只 warn)
    if path.exists() {
        tokio::fs::remove_dir_all(&path)
            .await
            .map_err(|e| AppError::system(format!("delete workspace failed: {e}")))?;
    }
    Ok(Json(json!({ "success": true, "deleted": true })))
}

// ── files-update ────────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FilesUpdateBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    c_id: String,
    files: Vec<code_service::FileOp>,
    #[serde(default)]
    custom_target_dir: Option<String>,
}

/// `POST /api/computer/files-update` (对齐 nuwax computer updateFiles; 增量 create/delete/rename/modify)。
#[utoipa::path(post, path = "/files-update", request_body = FilesUpdateBody, responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn files_update(
    State(state): State<AppState>,
    Json(mut body): Json<FilesUpdateBody>,
) -> Result<Json<Value>, AppError> {
    let path = resolve_computer_target(
        &state,
        &body.user_id,
        &body.c_id,
        body.custom_target_dir.as_deref(),
    )
    .await?;
    // 工作区不存在 → 创建 (对齐 nuwax computerFileUtils.updateFiles: !existsSync → mkdirSync recursive)。
    // 首次向全新 user/cId 工作区写入不应失败。
    tokio::fs::create_dir_all(&path).await?;
    // decodeURIComponent 文本内容 (对齐 nuwax safeDecodePath)
    for op in body.files.iter_mut() {
        if let Some(c) = op.contents.as_mut()
            && !c.is_empty()
        {
            *c = code_service::decode_uri_component(c);
        }
    }
    let count = body.files.len();
    // computer updateFiles: modify 用字节比较 (非 project 的行级 diff; 对齐 nuwax)
    code_service::apply_file_ops(
        &path,
        &body.files,
        code_service::ModifyStrategy::ByteCompare,
    )
    .await?;
    Ok(Json(json!({
        "success": true,
        "message": "User files updated successfully",
        "userId": body.user_id,
        "cId": body.c_id,
        "filesCount": count,
    })))
}

// ── upload-file / upload-files ──────────────────────────────────────────────────

/// `POST /api/computer/upload-file` (对齐 nuwax computer uploadFile; multipart)。
/// 返回 {success, message, fileSize} (不返回 filePath/originalname)。
#[utoipa::path(post, path = "/upload-file", request_body(content = UploadFileForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn upload_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut file_path = None;
    let mut custom_target_dir = None;
    let mut data = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "filePath" => file_path = Some(text_field(field).await?),
            "customTargetDir" => custom_target_dir = Some(text_field(field).await?),
            "file" => {
                data = Some(
                    file_field(
                        field,
                        state.config.upload_max_file_size_bytes,
                        &state.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                )
            }
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    let file_path = file_path.ok_or_else(|| AppError::validation("filePath is required"))?;
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;
    let ws = resolve_computer_target(&state, &user_id, &cid, custom_target_dir.as_deref()).await?;
    let target = path_safety::ensure_within(&ws, &file_path)?;
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let file_size = data.size();
    crate::service::temp_file::copy_file(data.path(), &target).await?;
    Ok(Json(json!({
        "success": true,
        "message": "File uploaded successfully",
        "fileSize": file_size,
    })))
}

// ── generate-file ───────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GenerateFileBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    c_id: String,
    /// 文件名，可含相对子路径 (如 "src/foo.txt")；对齐 nuwax normalizeFilePath 会剥离前导 `/`。
    file_name: String,
    /// 文本内容，缺省视为空串。
    #[serde(default)]
    content: Option<String>,
    /// 绝对目录覆盖；非空则用之，否则回退默认工作区 (与 upload-file 同语义)。
    #[serde(default)]
    custom_target_dir: Option<String>,
}

/// `POST /api/computer/generate-file` (对齐 nuwax computer generateFile, commit 9bea35e)。
///
/// 与 [`upload_file`] 区别：本接口接收 **JSON 文本** (`fileName` + `content`) 而非 multipart 上传，
/// 适用于 agent 直接由文本内容生成文件。复用 [`resolve_computer_target`] + [`path_safety::ensure_within`]
/// (路径穿越校验) + 父目录自动创建。
#[utoipa::path(
    post,
    path = "/generate-file",
    request_body = GenerateFileBody,
    responses(crate::openapi::JsonApiResponses),
    tag = "Computer"
)]
pub(crate) async fn generate_file(
    State(state): State<AppState>,
    Json(body): Json<GenerateFileBody>,
) -> Result<Json<Value>, AppError> {
    // 对齐 TS generateFile: normalizedFileName = fileName.trim() (空判断只看 trim 后是否空)。
    let normalized = body.file_name.trim();
    if normalized.is_empty() {
        return Err(AppError::validation("fileName cannot be empty"));
    }
    let content = body.content.unwrap_or_default();
    let ws = resolve_computer_target(
        &state,
        &body.user_id,
        &body.c_id,
        body.custom_target_dir.as_deref(),
    )
    .await?;
    // 对齐 TS uploadFile.normalizeFilePath: 路径拼接时剥离前导 `/`
    // (允许 "src/foo.txt" 这类相对子路径;绝对路径会被 ensure_within 拒)。
    let target = path_safety::ensure_within(&ws, normalized.trim_start_matches('/'))?;
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let bytes = content.as_bytes();
    let file_size = bytes.len();
    tokio::fs::write(&target, bytes)
        .await
        .map_err(|e| AppError::system(format!("write generated file failed: {e}")))?;
    Ok(Json(json!({
        "success": true,
        "message": "File generated successfully",
        "fileName": normalized,
        "fileSize": file_size,
    })))
}

/// `POST /api/computer/upload-files` (对齐 nuwax computer uploadFiles; 多文件 multipart)。
/// 返回 {success, message, totalCount, successCount, failCount, results:[{success,filePath,originalname?,message?,fileSize?,error?}]}。
#[utoipa::path(post, path = "/upload-files", request_body(content = UploadFilesForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn upload_files(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut custom_target_dir = None;
    let mut file_paths: Vec<String> = Vec::new();
    let mut files_vec = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "customTargetDir" => custom_target_dir = Some(text_field(field).await?),
            "filePaths" => file_paths.push(text_field(field).await?),
            "files" => {
                let original = field.file_name().map(|s| s.to_string());
                files_vec.push((
                    original,
                    file_field(
                        field,
                        state.config.upload_max_file_size_bytes,
                        &state.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                ));
            }
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    if file_paths.len() != files_vec.len() {
        return Err(AppError::validation("filePaths and files count mismatch"));
    }
    let ws = resolve_computer_target(&state, &user_id, &cid, custom_target_dir.as_deref()).await?;
    let total = file_paths.len();
    let mut success_count = 0usize;
    let mut results: Vec<Value> = Vec::new();
    for (fp, (original, data)) in file_paths.iter().zip(&files_vec) {
        let target = match path_safety::ensure_within(&ws, fp) {
            Ok(t) => t,
            Err(_) => {
                results.push(json!({
                    "success": false,
                    "filePath": fp,
                    "originalname": original,
                    "error": "Invalid file path",
                }));
                continue;
            }
        };
        let file_size = data.size();
        match write_file_create_parent(&target, data.path()).await {
            Ok(()) => {
                success_count += 1;
                results.push(json!({
                    "success": true,
                    "filePath": fp,
                    "originalname": original,
                    "message": "File uploaded successfully",
                    "fileSize": file_size,
                }));
            }
            Err(e) => {
                results.push(json!({
                    "success": false,
                    "filePath": fp,
                    "originalname": original,
                    "error": e.to_string(),
                }));
            }
        }
    }
    let fail_count = total - success_count;
    Ok(Json(json!({
        "success": true,
        "message": "Batch upload completed",
        "totalCount": total,
        "successCount": success_count,
        "failCount": fail_count,
        "results": results,
    })))
}

/// 写文件 (父目录自动创建); 用于 upload-files 单文件隔离错误。
async fn write_file_create_parent(target: &Path, source: &Path) -> Result<(), AppError> {
    crate::service::temp_file::copy_file(source, target)
        .await
        .map(|_| ())
}

// ── import-project ──────────────────────────────────────────────────────────────

/// `POST /api/computer/import-project` (对齐 nuwax computer importProject):
/// 上传 zip → 解压 + removeTopLevelDir + 白名单保留合并到工作区。
#[utoipa::path(post, path = "/import-project", request_body(content = ImportProjectForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn import_project(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut custom_target_dir = None;
    let mut data = None;
    let mut file_name = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "customTargetDir" => custom_target_dir = Some(text_field(field).await?),
            "file" => {
                file_name = field.file_name().map(|s| s.to_string());
                data = Some(
                    file_field(
                        field,
                        state.config.upload_max_file_size_bytes,
                        &state.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                );
            }
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;
    validate_zip_ext(file_name.as_deref())?;
    let target_dir =
        resolve_computer_target(&state, &user_id, &cid, custom_target_dir.as_deref()).await?;
    tokio::fs::create_dir_all(&target_dir).await?;
    let res = crate::service::computer_ws::import_project(&target_dir, data.path()).await?;
    Ok(Json(json!({
        "success": true,
        "message": "Project imported successfully",
        "userId": user_id,
        "cId": cid,
        "targetDir": res.target_dir,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::{
        AppState, BuildManager, BuildTaskStore, Config, DevServerManager, LocalWorkspaceResolver,
        LogCacheManager, SkillDownloader, WorkspaceResolver,
    };

    /// 构造一个指向临时目录的 AppState (computer root = temp)，镜像 FileServerBuilder::build。
    fn make_state(computer_root: std::path::PathBuf) -> AppState {
        let config = Arc::new(Config::default());
        let resolver: Arc<dyn WorkspaceResolver> = Arc::new(LocalWorkspaceResolver::new(
            config.project_source_dir.clone(),
            computer_root,
        ));
        AppState {
            resolver,
            dev_server: Arc::new(DevServerManager::new(config.clone())),
            build_manager: Arc::new(BuildManager::new(config.max_build_concurrency)),
            log_cache: Arc::new(LogCacheManager::new(&config)),
            skill_downloader: Arc::new(
                SkillDownloader::new(&config).expect("construct skill downloader"),
            ),
            build_tasks: Arc::new(BuildTaskStore::new()),
            config,
            started_at: std::time::Instant::now(),
        }
    }

    #[tokio::test]
    async fn generate_file_writes_content_and_creates_subdirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root.clone());
        let body = GenerateFileBody {
            user_id: "u".into(),
            c_id: "c".into(),
            file_name: "src/a.txt".into(),
            content: Some("hi".into()),
            custom_target_dir: None,
        };
        let res = generate_file(State(state), Json(body))
            .await
            .expect("generate-file should succeed");
        let val = res.0;
        assert_eq!(val["success"], serde_json::json!(true));
        assert_eq!(val["message"], "File generated successfully");
        assert_eq!(val["fileName"], "src/a.txt");
        assert_eq!(val["fileSize"], 2);
        let written = std::fs::read(computer_root.join("u").join("c").join("src").join("a.txt"))
            .expect("file written");
        assert_eq!(written, b"hi");
    }

    #[tokio::test]
    async fn generate_file_custom_target_dir_overrides_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let custom = tmp.path().join("custom-target");
        let state = make_state(tmp.path().join("c"));
        let body = GenerateFileBody {
            user_id: "u".into(),
            c_id: "c".into(),
            file_name: "top.txt".into(),
            content: Some("x".into()),
            custom_target_dir: Some(custom.to_string_lossy().into_owned()),
        };
        generate_file(State(state), Json(body))
            .await
            .expect("generate-file with customTargetDir");
        assert_eq!(
            std::fs::read(custom.join("top.txt")).expect("file at customTargetDir"),
            b"x"
        );
    }

    #[tokio::test]
    async fn generate_file_rejects_path_traversal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = make_state(tmp.path().join("c"));
        let body = GenerateFileBody {
            user_id: "u".into(),
            c_id: "c".into(),
            file_name: "../escape.txt".into(),
            content: Some("pwned".into()),
            custom_target_dir: None,
        };
        let err = generate_file(State(state), Json(body))
            .await
            .err()
            .expect("path traversal must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("not safe") || msg.contains("exceed"),
            "expected path-safety error, got: {msg}"
        );
        // 确保逃逸文件未落地
        assert!(!tmp.path().join("escape.txt").exists());
    }

    #[tokio::test]
    async fn generate_file_rejects_empty_file_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = make_state(tmp.path().join("c"));
        // 对齐 TS generateFile: 空判断只看 trim 后是否为空 (whitespace-only 也算空)
        let body = GenerateFileBody {
            user_id: "u".into(),
            c_id: "c".into(),
            file_name: "   ".into(),
            content: None,
            custom_target_dir: None,
        };
        let err = generate_file(State(state), Json(body))
            .await
            .err()
            .expect("empty fileName must be rejected");
        assert!(
            err.to_string().contains("fileName cannot be empty"),
            "expected empty-fileName error, got: {err}"
        );
    }

    #[tokio::test]
    async fn generate_file_strips_leading_slash_for_path_but_echoes_trimmed() {
        // 对齐 TS: fileName="/src/a.txt" → 写入 ws/src/a.txt (剥前导 /),
        // 但响应 fileName 回显 trim 后的 "/src/a.txt" (generateFile 返回 normalizedFileName)。
        let tmp = tempfile::tempdir().expect("tempdir");
        let computer_root = tmp.path().join("c");
        let state = make_state(computer_root.clone());
        let body = GenerateFileBody {
            user_id: "u".into(),
            c_id: "c".into(),
            file_name: "/src/a.txt".into(),
            content: Some("hi".into()),
            custom_target_dir: None,
        };
        let res = generate_file(State(state), Json(body))
            .await
            .expect("leading-slash fileName should succeed");
        let val = res.0;
        assert_eq!(val["fileName"], "/src/a.txt"); // 回显 trim 后(保留斜杠)
        assert_eq!(val["fileSize"], 2);
        // 实际写入剥前导斜杠
        let written = std::fs::read(computer_root.join("u").join("c").join("src").join("a.txt"))
            .expect("file written under src/");
        assert_eq!(written, b"hi");
    }

    // resolve_file / search_files handler 层测试已迁移至 files_read.rs。
}
