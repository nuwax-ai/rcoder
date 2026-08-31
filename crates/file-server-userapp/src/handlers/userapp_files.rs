//! `/api/v1/userapp` 文件操作镜像族（读+写）: computer 域同参镜像, workspace 走 UserApp 开发卷。
//!
//! 参数对齐 computer 域语义但自有命名: `app_id`（原 cId 改名）、`user_id`
//! （保留, 不参与路径, 为挂载分区组成段）。定位统一 `resolve_userapp_dev` =
//! `{USERAPP_WORKSPACE_DIR}/{app_id}`; 编排复用 file-server `*_core`（类型化
//! 业务核心），**响应在本壳层自拼 snake JSON**——computer 域 TS 驼峰契约
//! 不经本域（键风格分歧归各域拼装层，两域不复制编排防漂移）。

use axum::extract::State;
use garde::Validate;
use serde_json::{Value, json};

use crate::UserAppState;
use crate::models::{
    UserappFileEntry, UserappFileListQuery, UserappFilesUpdateBody, UserappGenerateFileBody,
    UserappImportProjectForm, UserappResolveFileQuery, UserappSearchFilesQuery,
    UserappUploadFileForm, UserappUploadFilesForm,
};
use file_server::error::AppError;
use file_server::extract::{AppJson as Json, AppMultipart as Multipart, AppQuery as Query};
use file_server::ops::files::files_update_core;
use file_server::ops::files::{
    BatchUploadItem, generate_file_core, import_project_core, upload_file_core, upload_files_core,
};
use file_server::ops::files_read::{get_file_list_core, resolve_file_core, search_files_core};
use file_server::ops::multipart::{file_field, text_field, validate_zip_ext};
use file_server::service::code as code_service;
use file_server::service::temp_file::TemporaryFile;
use file_server::workspace::resolve_userapp_dev;

// ── get-file-list ───────────────────────────────────────────────────────────────

/// 文件树列表（轻量元信息，不读内容）
///
/// 列开发卷 workspace 内指定目录的文件清单（名称/大小/mtime 等元信息，不读
/// 内容）。`relative_path` 相对 workspace 根（缺省列根一层）；`recursive`
/// 缺省 true 递归展开整棵子树，显式 "false" 仅当前层。响应
/// `{ success, files[], recursive }`；传 `proxy_path` 时条目附 `proxy_url`
/// 预览地址（`custom_target_dir` 非空时自动追加同名 query 参数）。
#[utoipa::path(
    get,
    path = "/get-file-list",
    params(UserappFileListQuery),
    responses(file_server::openapi::JsonApiResponses),
    tag = "UserApp · 双态 · 文件镜像"
)]
pub(crate) async fn get_file_list(
    State(state): State<UserAppState>,
    Query(q): Query<UserappFileListQuery>,
) -> Result<Json<Value>, AppError> {
    q.validate().map_err(file_server::error::from_garde)?;
    let path = resolve_userapp_dev(&q.app_id, q.custom_target_dir.as_deref(), &state.fs.config)?;
    let (entries, is_recursive) = get_file_list_core(
        &state.fs,
        &path,
        q.proxy_path.as_deref(),
        q.relative_path.as_deref(),
        q.recursive.as_deref(),
    )
    .await?;
    let mut files: Vec<UserappFileEntry> = entries.into_iter().map(Into::into).collect();
    append_proxy_url_suffix(&mut files, q.custom_target_dir.as_deref());
    Ok(Json(
        json!({ "success": true, "files": files, "recursive": is_recursive }),
    ))
}

// ── resolve-file ────────────────────────────────────────────────────────────────

/// 校验文件存在性，存在返回预览 URL
///
/// 探测 `file_path`（workspace 内相对路径，必填非空）指向的文件：不存在返回
/// `{ success, exists: false }`；存在返回 `{ success, exists: true, name,
/// file_proxy_url }`——`proxy_path` 为预览 URL 前缀（缺省则响应不含
/// file_proxy_url）；`custom_target_dir` 非空时自动追加
/// `?custom_target_dir=`（语义同 computer 域 customTargetDir 后缀）。
#[utoipa::path(
    get,
    path = "/resolve-file",
    params(UserappResolveFileQuery),
    responses(file_server::openapi::JsonApiResponses),
    tag = "UserApp · 双态 · 文件镜像"
)]
pub(crate) async fn resolve_file(
    State(state): State<UserAppState>,
    Query(q): Query<UserappResolveFileQuery>,
) -> Result<Json<Value>, AppError> {
    q.validate().map_err(file_server::error::from_garde)?;
    let path = resolve_userapp_dev(&q.app_id, q.custom_target_dir.as_deref(), &state.fs.config)?;
    let mut r = match resolve_file_core(path, q.file_path.trim(), q.proxy_path.as_deref()).await? {
        Some(r) => r,
        None => return Ok(Json(json!({ "success": true, "exists": false }))),
    };
    // file_proxy_url 追加 ?custom_target_dir=（语义同 computer 域 customTargetDir 后缀）
    if let (Some(ct), Some(url)) = (
        trimmed_non_empty(q.custom_target_dir.as_deref()),
        r.file_proxy_url.as_mut(),
    ) {
        url.push_str("?custom_target_dir=");
        url.push_str(&code_service::encode_uri_component(ct));
    }
    Ok(Json(json!({
        "success": true,
        "exists": true,
        "name": r.name,
        "file_proxy_url": r.file_proxy_url,
    })))
}

// ── search-files ────────────────────────────────────────────────────────────────

/// 无索引有界实时搜索
///
/// 按 `kw`（文件名/相对路径子串，大小写不敏感）遍历 workspace 实时匹配，
/// 三重上限防大目录失控：`limit` 命中条数上限、`max_visit` 访问条目硬上限
/// （含未命中）、`timeout_ms` 超时毫秒。响应 `{ success, files[], truncated,
/// visited }`——`truncated=true` 表示因上限/超时提前结束、结果不完整，
/// 调用方应提示而非当作全量。
#[utoipa::path(
    get,
    path = "/search-files",
    params(UserappSearchFilesQuery),
    responses(file_server::openapi::JsonApiResponses),
    tag = "UserApp · 双态 · 文件镜像"
)]
pub(crate) async fn search_files(
    State(state): State<UserAppState>,
    Query(q): Query<UserappSearchFilesQuery>,
) -> Result<Json<Value>, AppError> {
    q.validate().map_err(file_server::error::from_garde)?;
    let path = resolve_userapp_dev(&q.app_id, q.custom_target_dir.as_deref(), &state.fs.config)?;
    let r = search_files_core(
        &state.fs,
        path,
        file_server::ops::files_read::SearchFilesParams {
            proxy_path: q.proxy_path.as_deref(),
            relative_path: q.relative_path.as_deref(),
            kw: q.kw.trim(),
            limit: &q.limit,
            max_visit: &q.max_visit,
            timeout_ms: &q.timeout_ms,
            custom_target_dir: q.custom_target_dir.as_deref(),
        },
    )
    .await?;
    let mut files: Vec<UserappFileEntry> = r.files.into_iter().map(Into::into).collect();
    append_proxy_url_suffix(&mut files, q.custom_target_dir.as_deref());
    Ok(Json(json!({
        "success": true,
        "files": files,
        "truncated": r.truncated,
        "visited": r.visited,
    })))
}

// ── files-update ────────────────────────────────────────────────────────────────

/// 批量文件增删改（modify 字节比较）
///
/// 对 `files` 数组逐项执行 create / delete / rename / modify（`operation`
/// 字段区分；rename 须带 `rename_from`，create/modify 携 `contents` 文本、
/// 服务端做 URL 解码）。modify 以字节比较判变更，内容相同跳过写入。
/// 响应 `{ success, message, user_id, app_id, files_count }`（files_count
/// = 实际执行动作数）。注意 delete 按路径直删无回收站，调用方自行确认。
#[utoipa::path(post, path = "/files-update", request_body = UserappFilesUpdateBody, responses(file_server::openapi::JsonApiResponses), tag = "UserApp · 双态 · 文件镜像")]
pub(crate) async fn files_update(
    State(state): State<UserAppState>,
    Json(body): Json<UserappFilesUpdateBody>,
) -> Result<Json<Value>, AppError> {
    let path = resolve_userapp_dev(
        &body.app_id,
        body.custom_target_dir.as_deref(),
        &state.fs.config,
    )?;
    let files: Vec<_> = body.files.into_iter().map(Into::into).collect();
    let count = files_update_core(&path, files).await?;
    Ok(Json(json!({
        "success": true,
        "message": "User files updated successfully",
        "user_id": body.user_id,
        "app_id": body.app_id,
        "files_count": count,
    })))
}

// ── upload-file / upload-files ──────────────────────────────────────────────────

/// 单文件上传（multipart）
///
/// 上传单个文件到开发卷：`file_path` 指定 workspace 内相对路径，按原字节
/// 直写落盘（不解析压缩包——zip/tar.gz 保存为文件本体）。`custom_target_dir`
/// 可覆盖 workspace 根（Java 侧负责合法性）。
#[utoipa::path(post, path = "/upload-file", request_body(content = UserappUploadFileForm, content_type = "multipart/form-data"), responses(file_server::openapi::JsonApiResponses), tag = "UserApp · 双态 · 文件镜像")]
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
            "app_id" => app_id = Some(text_field(field).await?),
            "user_id" => user_id = Some(text_field(field).await?),
            "file_path" => file_path = Some(text_field(field).await?),
            "custom_target_dir" => custom_target_dir = Some(text_field(field).await?),
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
    let file_path = require_app_field(file_path, "file_path")?;
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;
    tracing::debug!(app_id = %app_id, user_id = %user_id, "userapp upload-file");
    let ws = resolve_userapp_dev(&app_id, custom_target_dir.as_deref(), &state.fs.config)?;
    let r = upload_file_core(&ws, &file_path, data).await?;
    Ok(Json(json!({
        "success": true,
        "message": "File uploaded successfully",
        "file_size": r.file_size,
    })))
}

/// 多文件上传（单文件错误隔离）
///
/// multipart 重复字段批量上传：`file_paths` 与 `files` 一一对应落盘；
/// 单个文件失败不影响其余（错误隔离，逐项返回结果）。
#[utoipa::path(post, path = "/upload-files", request_body(content = UserappUploadFilesForm, content_type = "multipart/form-data"), responses(file_server::openapi::JsonApiResponses), tag = "UserApp · 双态 · 文件镜像")]
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
            "app_id" => app_id = Some(text_field(field).await?),
            "user_id" => user_id = Some(text_field(field).await?),
            "custom_target_dir" => custom_target_dir = Some(text_field(field).await?),
            "file_paths" => file_paths.push(text_field(field).await?),
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
    let app_id = require_app_field(app_id, "app_id")?;
    let user_id = require_app_field(user_id, "user_id")?;
    tracing::debug!(app_id = %app_id, user_id, "userapp upload-files");
    if file_paths.len() != files_vec.len() {
        return Err(AppError::validation("file_paths and files count mismatch"));
    }
    let ws = resolve_userapp_dev(&app_id, custom_target_dir.as_deref(), &state.fs.config)?;
    let r = upload_files_core(&ws, &file_paths, &files_vec).await?;
    let results: Vec<Value> = r
        .results
        .into_iter()
        .map(|item| match item {
            BatchUploadItem::Ok {
                file_path,
                original,
                file_size,
            } => json!({
                "success": true,
                "file_path": file_path,
                "originalname": original,
                "message": "File uploaded successfully",
                "file_size": file_size,
            }),
            BatchUploadItem::Err {
                file_path,
                original,
                error,
            } => json!({
                "success": false,
                "file_path": file_path,
                "originalname": original,
                "error": error,
            }),
        })
        .collect();
    Ok(Json(json!({
        "success": true,
        "message": "Batch upload completed",
        "total_count": r.total,
        "success_count": r.success_count,
        "fail_count": r.total - r.success_count,
        "results": results,
    })))
}

// ── generate-file ───────────────────────────────────────────────────────────────

/// JSON 文本生成文件
///
/// 以 `file_name`（可含相对子路径，自动剥前导 `/`）在 workspace 内写入
/// `content` 文本（缺省空串）；父目录自动创建。适合生成配置/代码骨架等
/// 纯文本产物。
#[utoipa::path(
    post,
    path = "/generate-file",
    request_body = UserappGenerateFileBody,
    responses(file_server::openapi::JsonApiResponses),
    tag = "UserApp · 双态 · 文件镜像"
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
    let r = generate_file_core(ws, body.file_name.trim(), body.content.unwrap_or_default()).await?;
    Ok(Json(json!({
        "success": true,
        "message": "File generated successfully",
        "file_name": r.file_name,
        "file_size": r.file_size,
    })))
}

// ── import-project ──────────────────────────────────────────────────────────────

/// 项目 zip 导入开发卷（解压合并）
///
/// 上传项目 zip 解压合并到开发卷 workspace（`file` 必填；zip 按魔数识别
/// 解压，单文件直写）；`custom_target_dir` 可覆盖 workspace 根。
#[utoipa::path(post, path = "/import-project", request_body(content = UserappImportProjectForm, content_type = "multipart/form-data"), responses(file_server::openapi::JsonApiResponses), tag = "UserApp · 双态 · 文件镜像")]
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
            "app_id" => app_id = Some(text_field(field).await?),
            "user_id" => user_id = Some(text_field(field).await?),
            "custom_target_dir" => custom_target_dir = Some(text_field(field).await?),
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
    let app_id = require_app_field(app_id, "app_id")?;
    let user_id = require_app_field(user_id, "user_id")?;
    let data: TemporaryFile = data.ok_or_else(|| AppError::validation("file is required"))?;
    validate_zip_ext(file_name.as_deref())?;
    let ws = resolve_userapp_dev(&app_id, custom_target_dir.as_deref(), &state.fs.config)?;
    let target = import_project_core(ws, data).await?;
    Ok(Json(json!({
        "success": true,
        "message": "Project imported successfully",
        "user_id": user_id,
        "app_id": app_id,
        "target_dir": target,
    })))
}

/// 预览 URL 追加 custom_target_dir 后缀（list/search 条目统一处理）。
fn append_proxy_url_suffix(files: &mut [UserappFileEntry], custom_target_dir: Option<&str>) {
    let Some(ct) = trimmed_non_empty(custom_target_dir) else {
        return;
    };
    let suffix = format!(
        "?custom_target_dir={}",
        code_service::encode_uri_component(ct)
    );
    for f in files.iter_mut() {
        if let Some(u) = f.file_proxy_url.as_mut() {
            u.push_str(&suffix);
        }
    }
}

/// trim 后非空才返回（custom_target_dir 的 URL 后缀语义）。
fn trimmed_non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// multipart 提取后必填字段校验 (空/缺失 → 400, 错误消息带字段名便于定位)。
pub(crate) fn require_app_field(value: Option<String>, field: &str) -> Result<String, AppError> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::validation(format!("{field} is required (missing or blank)")))
}

/// 测试夹具（handlers 域共享）：userapp AppState 构造。
#[cfg(test)]
pub(crate) mod tests_support {
    use std::sync::Arc;

    use crate::UserAppState;
    use crate::service::userapp::tasks::BuildTaskStore;
    use file_server::{
        BuildManager, Config, DevServerManager, LocalWorkspaceResolver, LogCacheManager,
        SkillDownloader, WorkspaceResolver,
    };

    /// userapp 版 make_state: `userapp_workspace_dir` 指向 tempdir (resolve_userapp_dev 的根)。
    pub(crate) fn make_state(userapp_root: std::path::PathBuf) -> UserAppState {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Path;

    use file_server::extract::AppJson;

    use super::tests_support::make_state;

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
        file_server::ops::workspace::init_project_template_core(&state.fs, ws, data, false)
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

        // 3. detect_project ({app_id}/{app_stage} 新形态) 应在开发卷里找到项目
        let reply = super::super::userapp::detect_project(
            State(state),
            Path(("app-1".into(), "dev".into())),
            AppJson(crate::models::ProjectChainBody {
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
        assert_eq!(res.0["file_proxy_url"], "/proxy/src/a.txt");
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
                files: vec![crate::models::UserappFileOp {
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
        assert_eq!(res.0["app_id"], "app-3");
        assert_eq!(
            std::fs::read(tmp.path().join("app-3").join("pkg.json")).unwrap(),
            b"{}"
        );
    }

    /// resolve_userapp_dev: customTargetDir 信任覆盖 + identifier 路径穿越拒绝。
    #[test]
    fn resolve_userapp_dev_semantics() {
        let config = file_server::Config {
            userapp_workspace_dir: std::path::PathBuf::from("/data/userapp"),
            ..file_server::Config::default()
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
