//! `/api/userapp` 文件操作镜像族（读+写）: computer 域同参镜像, workspace 走 UserApp 开发卷。
//!
//! 参数对齐 computer 域语义但自有命名: `appId`（≡app_id, 原 cId 改名）、`userId`
//! （保留, 不参与路径, 仅审计日志）。定位统一 `resolve_userapp_dev` =
//! `{USERAPP_WORKSPACE_DIR}/{appId}`; 核心逻辑复用 computer 域 impl（bac9663 抽取）,
//! 两域不复制实现防漂移。

use axum::extract::State;
use garde::Validate;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::UserAppState;
use file_server::error::AppError;
use file_server::extract::{AppJson as Json, AppMultipart as Multipart, AppQuery as Query};
use file_server::handlers::computer::files::files_update_impl;
use file_server::handlers::computer::files::generate::generate_file_impl;
use file_server::handlers::computer::files::import_project::import_project_impl;
use file_server::handlers::computer::files::upload::{upload_file_impl, upload_files_impl};
use file_server::handlers::computer::files_read::{
    FileListParams, SearchFilesParams, get_file_list_impl, resolve_file_impl, search_files_impl,
};
use file_server::handlers::multipart::{file_field, text_field, validate_zip_ext};
use file_server::service::code as code_service;
use file_server::service::temp_file::TemporaryFile;
use file_server::workspace::resolve_userapp_dev;

// ── get-file-list ───────────────────────────────────────────────────────────────

/// userapp 版 get-file-list 查询参数 (computer FileListQuery 镜像, cId→appId)。
#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserappFileListQuery {
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// UserApp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{appId}`）
    pub app_id: String,
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// 用户 ID（审计字段，不参与路径定位）
    pub user_id: String,
    #[serde(default)]
    #[garde(skip)]
    /// 预览 URL 前缀（fileProxyUrl 的 base）；缺省则响应不含 fileProxyUrl
    pub proxy_path: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    /// 相对 workspace 根的子目录（可多级）；缺省列根目录
    pub relative_path: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    /// 是否递归展开子目录；缺省 true，显式 "false" 仅当前层
    pub recursive: Option<String>,
}

/// 文件树列表（轻量元信息，不读内容）
#[utoipa::path(
    get,
    path = "/get-file-list",
    params(UserappFileListQuery),
    responses(file_server::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn get_file_list(
    State(state): State<UserAppState>,
    Query(q): Query<UserappFileListQuery>,
) -> Result<Json<Value>, AppError> {
    q.validate().map_err(file_server::error::from_garde)?;
    let path = resolve_userapp_dev(&q.app_id, q.custom_target_dir.as_deref(), &state.fs.config)?;
    get_file_list_impl(
        &state.fs,
        &path,
        FileListParams {
            proxy_path: q.proxy_path.as_deref(),
            relative_path: q.relative_path.as_deref(),
            recursive: q.recursive.as_deref(),
            custom_target_dir: q.custom_target_dir.as_deref(),
        },
    )
    .await
}

// ── resolve-file ────────────────────────────────────────────────────────────────

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserappResolveFileQuery {
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// UserApp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{appId}`）
    pub app_id: String,
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// 用户 ID（审计字段，不参与路径定位）
    pub user_id: String,
    #[serde(default)]
    #[garde(skip)]
    /// 预览 URL 前缀（fileProxyUrl 的 base）；缺省则响应不含 fileProxyUrl
    pub proxy_path: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// workspace 内相对路径的文件（必填非空）
    pub file_path: String,
}

/// 校验文件存在性，存在返回预览 URL
#[utoipa::path(
    get,
    path = "/resolve-file",
    params(UserappResolveFileQuery),
    responses(file_server::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn resolve_file(
    State(state): State<UserAppState>,
    Query(q): Query<UserappResolveFileQuery>,
) -> Result<Json<Value>, AppError> {
    q.validate().map_err(file_server::error::from_garde)?;
    let path = resolve_userapp_dev(&q.app_id, q.custom_target_dir.as_deref(), &state.fs.config)?;
    resolve_file_impl(
        path,
        q.file_path.trim(),
        q.proxy_path.as_deref(),
        q.custom_target_dir.as_deref(),
    )
    .await
}

// ── search-files ────────────────────────────────────────────────────────────────

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserappSearchFilesQuery {
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// UserApp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{appId}`）
    pub app_id: String,
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// 用户 ID（审计字段，不参与路径定位）
    pub user_id: String,
    #[serde(default)]
    #[garde(skip)]
    /// 预览 URL 前缀（fileProxyUrl 的 base）；缺省则响应不含 fileProxyUrl
    pub proxy_path: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    /// 相对 workspace 根的子目录（可多级）；缺省列根目录
    pub relative_path: Option<String>,
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// 搜索关键字（文件名/相对路径子串，大小写不敏感；必填非空）
    pub kw: String,
    #[garde(custom(file_server::validation_rules::positive_int))]
    /// 命中条数上限（必填正整数）
    pub limit: String,
    #[garde(custom(file_server::validation_rules::positive_int))]
    /// 访问条目数硬上限，含未命中（必填正整数）
    pub max_visit: String,
    #[garde(custom(file_server::validation_rules::positive_int))]
    /// 超时毫秒数（必填正整数）
    pub timeout_ms: String,
}

/// 无索引有界实时搜索
#[utoipa::path(
    get,
    path = "/search-files",
    params(UserappSearchFilesQuery),
    responses(file_server::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn search_files(
    State(state): State<UserAppState>,
    Query(q): Query<UserappSearchFilesQuery>,
) -> Result<Json<Value>, AppError> {
    q.validate().map_err(file_server::error::from_garde)?;
    let path = resolve_userapp_dev(&q.app_id, q.custom_target_dir.as_deref(), &state.fs.config)?;
    search_files_impl(
        &state.fs,
        path,
        SearchFilesParams {
            proxy_path: q.proxy_path.as_deref(),
            relative_path: q.relative_path.as_deref(),
            kw: q.kw.trim(),
            limit: &q.limit,
            max_visit: &q.max_visit,
            timeout_ms: &q.timeout_ms,
            custom_target_dir: q.custom_target_dir.as_deref(),
        },
    )
    .await
}

// ── files-update ────────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserappFilesUpdateBody {
    #[serde(deserialize_with = "file_server::extract::deserialize_id_string")]
    /// UserApp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{appId}`）
    pub app_id: String,
    #[serde(deserialize_with = "file_server::extract::deserialize_id_string")]
    /// 用户 ID（审计字段，不参与路径定位）
    pub user_id: String,
    /// 上传文件的二进制内容（重复字段，与 filePaths 一一对应）
    pub files: Vec<code_service::FileOp>,
    #[serde(default)]
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
}

/// 批量文件增删改（modify 字节比较）
#[utoipa::path(post, path = "/files-update", request_body = UserappFilesUpdateBody, responses(file_server::openapi::JsonApiResponses), tag = "UserApp")]
pub(crate) async fn files_update(
    State(state): State<UserAppState>,
    Json(body): Json<UserappFilesUpdateBody>,
) -> Result<Json<Value>, AppError> {
    let path = resolve_userapp_dev(
        &body.app_id,
        body.custom_target_dir.as_deref(),
        &state.fs.config,
    )?;
    let count = files_update_impl(&path, body.files).await?;
    Ok(Json(json!({
        "success": true,
        "message": "User files updated successfully",
        "userId": body.user_id,
        "appId": body.app_id,
        "filesCount": count,
    })))
}

// ── upload-file / upload-files ──────────────────────────────────────────────────

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserappUploadFileForm {
    /// UserApp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{appId}`）
    pub app_id: String,
    /// 用户 ID（审计字段，不参与路径定位）
    pub user_id: String,
    /// workspace 内相对路径的文件（必填非空）
    pub file_path: String,
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
    #[schema(format = Binary)]
    /// 上传文件（zip 或单文件）
    pub file: String,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserappUploadFilesForm {
    /// UserApp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{appId}`）
    pub app_id: String,
    /// 用户 ID（审计字段，不参与路径定位）
    pub user_id: String,
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
    /// 每个文件的目标相对路径（与 files 一一对应，重复字段）
    pub file_paths: Vec<String>,
    /// 上传文件的二进制内容（重复字段，与 filePaths 一一对应）
    pub files: Vec<file_server::openapi::BinaryFile>,
}

/// 单文件上传（multipart）
#[utoipa::path(post, path = "/upload-file", request_body(content = UserappUploadFileForm, content_type = "multipart/form-data"), responses(file_server::openapi::JsonApiResponses), tag = "UserApp")]
pub(crate) async fn upload_file(
    State(state): State<UserAppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut app_id = None;
    let mut user_id = None;
    let mut file_path = None;
    let mut custom_target_dir = None;
    let mut data = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "appId" => app_id = Some(text_field(field).await?),
            "userId" => user_id = Some(text_field(field).await?),
            "filePath" => file_path = Some(text_field(field).await?),
            "customTargetDir" => custom_target_dir = Some(text_field(field).await?),
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
    let file_path = require_app_field(file_path, "filePath")?;
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;
    tracing::debug!(app_id = %app_id, user_id = %user_id, "userapp upload-file");
    let ws = resolve_userapp_dev(&app_id, custom_target_dir.as_deref(), &state.fs.config)?;
    upload_file_impl(&ws, &file_path, data).await
}

/// 多文件上传（单文件错误隔离）
#[utoipa::path(post, path = "/upload-files", request_body(content = UserappUploadFilesForm, content_type = "multipart/form-data"), responses(file_server::openapi::JsonApiResponses), tag = "UserApp")]
pub(crate) async fn upload_files(
    State(state): State<UserAppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut app_id = None;
    let mut user_id = None;
    let mut custom_target_dir = None;
    let mut file_paths: Vec<String> = Vec::new();
    let mut files_vec = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "appId" => app_id = Some(text_field(field).await?),
            "userId" => user_id = Some(text_field(field).await?),
            "customTargetDir" => custom_target_dir = Some(text_field(field).await?),
            "filePaths" => file_paths.push(text_field(field).await?),
            "files" => {
                let original = field.file_name().map(|s| s.to_string());
                files_vec.push((
                    original,
                    file_field(
                        field,
                        state.fs.config.upload_max_file_size_bytes,
                        &state.fs.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                ));
            }
            _ => {}
        }
    }
    let app_id = require_app_field(app_id, "appId")?;
    let user_id = require_app_field(user_id, "userId")?;
    tracing::debug!(app_id = %app_id, user_id, "userapp upload-files");
    if file_paths.len() != files_vec.len() {
        return Err(AppError::validation("filePaths and files count mismatch"));
    }
    let ws = resolve_userapp_dev(&app_id, custom_target_dir.as_deref(), &state.fs.config)?;
    upload_files_impl(&ws, &file_paths, &files_vec).await
}

// ── generate-file ───────────────────────────────────────────────────────────────

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserappGenerateFileBody {
    #[serde(deserialize_with = "file_server::extract::deserialize_id_string")]
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// UserApp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{appId}`）
    pub app_id: String,
    #[serde(deserialize_with = "file_server::extract::deserialize_id_string")]
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// 用户 ID（审计字段，不参与路径定位）
    pub user_id: String,
    #[garde(custom(file_server::validation_rules::not_blank))]
    /// 文件名，可含相对子路径（如 "src/foo.txt"；自动剥前导 `/`）
    pub file_name: String,
    #[serde(default)]
    /// 文本内容；缺省视为空串
    pub content: Option<String>,
    #[serde(default)]
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
}

/// JSON 文本生成文件
#[utoipa::path(
    post,
    path = "/generate-file",
    request_body = UserappGenerateFileBody,
    responses(file_server::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn generate_file(
    State(state): State<UserAppState>,
    Json(body): Json<UserappGenerateFileBody>,
) -> Result<Json<Value>, AppError> {
    body.validate().map_err(file_server::error::from_garde)?;
    let ws = resolve_userapp_dev(
        &body.app_id,
        body.custom_target_dir.as_deref(),
        &state.fs.config,
    )?;
    generate_file_impl(ws, body.file_name.trim(), body.content.unwrap_or_default()).await
}

// ── import-project ──────────────────────────────────────────────────────────────

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserappImportProjectForm {
    /// UserApp 应用 ID（workspace 定位 = `{USERAPP_WORKSPACE_DIR}/{appId}`）
    pub app_id: String,
    /// 用户 ID（审计字段，不参与路径定位）
    pub user_id: String,
    /// 目标根目录覆盖；trim 后非空则直接信任作为 workspace 根（Java 侧负责合法性）
    pub custom_target_dir: Option<String>,
    #[schema(format = Binary)]
    /// 上传文件（zip 或单文件）
    pub file: String,
}

/// 上传项目 zip 解压合并到开发卷 workspace
#[utoipa::path(post, path = "/import-project", request_body(content = UserappImportProjectForm, content_type = "multipart/form-data"), responses(file_server::openapi::JsonApiResponses), tag = "UserApp")]
pub(crate) async fn import_project(
    State(state): State<UserAppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut app_id = None;
    let mut user_id = None;
    let mut custom_target_dir = None;
    let mut data = None;
    let mut file_name = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "appId" => app_id = Some(text_field(field).await?),
            "userId" => user_id = Some(text_field(field).await?),
            "customTargetDir" => custom_target_dir = Some(text_field(field).await?),
            "file" => {
                file_name = field.file_name().map(|s| s.to_string());
                data = Some(
                    file_field(
                        field,
                        state.fs.config.upload_max_file_size_bytes,
                        &state.fs.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                );
            }
            _ => {}
        }
    }
    let app_id = require_app_field(app_id, "appId")?;
    let user_id = require_app_field(user_id, "userId")?;
    let data: TemporaryFile = data.ok_or_else(|| AppError::validation("file is required"))?;
    validate_zip_ext(file_name.as_deref())?;
    let ws = resolve_userapp_dev(&app_id, custom_target_dir.as_deref(), &state.fs.config)?;
    let target = import_project_impl(ws, data).await?;
    Ok(Json(json!({
        "success": true,
        "message": "Project imported successfully",
        "userId": user_id,
        "appId": app_id,
        "targetDir": target,
    })))
}

/// multipart 提取后必填字段校验 (空/缺失 → 400, 错误消息带字段名便于定位)。
pub(crate) fn require_app_field(value: Option<String>, field: &str) -> Result<String, AppError> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::validation(format!("{field} is required (missing or blank)")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::UserAppState;
    use crate::service::userapp::tasks::BuildTaskStore;
    use file_server::extract::AppJson;
    use file_server::{
        BuildManager, Config, DevServerManager, LocalWorkspaceResolver, LogCacheManager,
        SkillDownloader, WorkspaceResolver,
    };

    /// userapp 版 make_state: `userapp_workspace_dir` 指向 tempdir (resolve_userapp_dev 的根)。
    fn make_state(userapp_root: std::path::PathBuf) -> UserAppState {
        let config = Arc::new(Config {
            userapp_workspace_dir: userapp_root,
            ..Config::default()
        });
        let resolver: Arc<dyn WorkspaceResolver> = Arc::new(LocalWorkspaceResolver::new(
            config.project_source_dir.clone(),
            config.computer_workspace_dir.clone(),
        ));
        let fs = file_server::AppState {
            resolver,
            dev_server: Arc::new(DevServerManager::new(config.clone())),
            build_manager: Arc::new(BuildManager::new(config.max_build_concurrency)),
            log_cache: Arc::new(LogCacheManager::new(&config)),
            skill_downloader: Arc::new(
                SkillDownloader::new(&config).expect("construct skill downloader"),
            ),
            config,
            started_at: std::time::Instant::now(),
        };
        UserAppState {
            fs,
            build_tasks: Arc::new(BuildTaskStore::new()),
        }
    }

    /// 经公共 Writer API 构造 TemporaryFile (与 multipart file_field 同路径), 内容为单层 zip。
    async fn make_temp_zip(entries: &[(&str, &str)], parent: &std::path::Path) -> TemporaryFile {
        use std::io::Write;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        for (name, content) in entries {
            zw.start_file(name.to_string(), opts).unwrap();
            zw.write_all(content.as_bytes()).unwrap();
        }
        let bytes = zw.finish().unwrap().into_inner();
        let mut writer = file_server::service::temp_file::TemporaryFileWriter::create(
            parent,
            "test-zip-",
            u64::MAX,
        )
        .await
        .expect("create temp writer");
        writer
            .write(&bytes::Bytes::from(bytes))
            .await
            .expect("write zip bytes");
        writer.finish().await.expect("finish temp file")
    }

    /// 闭环铁证: init-project-template 与 get-file-list / detect_project 读同一棵开发卷。
    #[tokio::test]
    async fn init_file_list_detect_share_dev_volume() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = make_state(tmp.path().to_path_buf());

        // 1. init: 定位 = resolve_userapp_dev (与镜像壳一致), 模板含 react 项目
        let data = make_temp_zip(
            &[
                (
                    "demo-app/package.json",
                    r#"{"name":"demo","scripts":{"dev":"vite"}}"#,
                ),
                ("demo-app/src/main.tsx", "export {}\n"),
            ],
            &tmp.path().join("zips"),
        )
        .await;
        let ws = resolve_userapp_dev("app-1", None, &state.fs.config).unwrap();
        file_server::handlers::computer::workspace::init_template::init_project_template_impl(
            &state.fs, ws, data, false,
        )
        .await
        .expect("init template");

        // 2. get-file-list (镜像壳 Query handler) 应看到模板文件
        let q = Query(UserappFileListQuery {
            app_id: "app-1".into(),
            user_id: "u".into(),
            proxy_path: None,
            custom_target_dir: None,
            relative_path: None,
            recursive: Some("false".into()),
        });
        let res = get_file_list(State(state.clone()), q)
            .await
            .expect("list ok");
        let names: Vec<&str> = res.0["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"demo-app"), "names={names:?}");

        // 3. detect_project (存量接口, 已切开发卷) 应在开发卷里找到项目
        let reply = super::super::userapp::detect_project(
            State(state),
            AppJson(super::super::userapp::ImportProjectBody {
                app_id: "app-1".into(),
                user_id: "u1".into(),
                project_dir: "demo-app".into(),
            }),
        )
        .await;
        let crate::handlers::userapp::UserAppReply::Ok(data) = reply else {
            panic!("detect should succeed");
        };
        assert!(!data.detection.detected_type.is_empty());
        assert!(data.detection.manifest.contains("schema_version = 1"));
    }

    /// generate-file (Json 壳) → resolve-file (Query 壳) 往返, 落点在开发卷。
    #[tokio::test]
    async fn generate_then_resolve_roundtrip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = make_state(tmp.path().to_path_buf());
        generate_file(
            State(state.clone()),
            Json(UserappGenerateFileBody {
                app_id: "app-2".into(),
                user_id: "u".into(),
                file_name: "src/a.txt".into(),
                content: Some("hi".into()),
                custom_target_dir: None,
            }),
        )
        .await
        .expect("generate ok");
        // 物理落点断言: 开发卷 {root}/{app_id}/src/a.txt
        assert_eq!(
            std::fs::read(tmp.path().join("app-2").join("src").join("a.txt")).unwrap(),
            b"hi"
        );
        let res = resolve_file(
            State(state),
            Query(UserappResolveFileQuery {
                app_id: "app-2".into(),
                user_id: "u".into(),
                proxy_path: Some("/proxy".into()),
                custom_target_dir: None,
                file_path: "src/a.txt".into(),
            }),
        )
        .await
        .expect("resolve ok");
        assert_eq!(res.0["exists"], serde_json::json!(true));
        assert_eq!(res.0["fileProxyUrl"], "/proxy/src/a.txt");
    }

    /// files-update (Json 壳) 写入开发卷 + 响应回显 appId。
    #[tokio::test]
    async fn files_update_writes_into_dev_volume() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = make_state(tmp.path().to_path_buf());
        let res = files_update(
            State(state),
            Json(UserappFilesUpdateBody {
                app_id: "app-3".into(),
                user_id: "u".into(),
                files: vec![code_service::FileOp {
                    operation: "create".into(),
                    name: "pkg.json".into(),
                    is_dir: None,
                    contents: Some("{}".into()),
                    rename_from: None,
                }],
                custom_target_dir: None,
            }),
        )
        .await
        .expect("update ok");
        assert_eq!(res.0["success"], serde_json::json!(true));
        assert_eq!(res.0["appId"], "app-3");
        assert_eq!(
            std::fs::read(tmp.path().join("app-3").join("pkg.json")).unwrap(),
            b"{}"
        );
    }

    /// resolve_userapp_dev: customTargetDir 信任覆盖 + identifier 路径穿越拒绝。
    #[test]
    fn resolve_userapp_dev_semantics() {
        let config = Config {
            userapp_workspace_dir: std::path::PathBuf::from("/data/userapp"),
            ..Config::default()
        };
        // 常规: {root}/{appId}
        assert_eq!(
            resolve_userapp_dev("my-app", None, &config).unwrap(),
            std::path::PathBuf::from("/data/userapp/my-app")
        );
        // customTargetDir 信任 (trim 非空)
        assert_eq!(
            resolve_userapp_dev("my-app", Some("  /anywhere  "), &config).unwrap(),
            std::path::PathBuf::from("/anywhere")
        );
        // 空白 customTargetDir 视为未设置
        assert_eq!(
            resolve_userapp_dev("my-app", Some("   "), &config).unwrap(),
            std::path::PathBuf::from("/data/userapp/my-app")
        );
        // 路径穿越拒绝
        assert!(resolve_userapp_dev("../escape", None, &config).is_err());
    }
}
