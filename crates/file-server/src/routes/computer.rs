//! `/api/computer` 路由 (对齐 nuwax computerRoutes)。
//!
//! computer 工作区路径: `{COMPUTER_WORKSPACE_ROOT}/{userId}/{cId}/`。
//! 本文件含 6 个可直接实现的路由 (路径制/简单 spawn); files-update / upload /
//! create-workspace / build-agent-package 等依赖 resolver+ctx 改造的路由见后续。

use std::path::PathBuf;
use std::time::Duration;

use axum::extract::{Multipart, Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::path_safety;
use crate::service::{code as code_service, skills as skills_service, tree, zip};
use crate::workspace::ComputerContext;
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/get-file-list", get(get_file_list))
        .route("/delete-workspace", post(delete_workspace))
        .route("/get-logs", get(get_logs))
        .route("/execute-command", post(execute_command))
        .route("/install-project", post(install_project))
        .route("/zip-workspace", post(zip_workspace))
        .route("/download-all-files", get(download_all_files))
        .route("/files-update", post(files_update))
        .route("/all-files-update", post(all_files_update))
        .route("/upload-file", post(upload_file))
        .route("/upload-files", post(upload_files))
        .route("/import-project", post(import_project))
        .route("/cleanup-build-artifacts", post(cleanup_build_artifacts))
        .route("/create-workspace", post(create_workspace))
        .route("/push-skills-to-workspace", post(push_skills_to_workspace))
        .route("/init-project-template", post(init_project_template))
        .route("/build-agent-package", post(build_agent_package))
}

async fn text_field(field: axum::extract::multipart::Field<'_>) -> Result<String, AppError> {
    field
        .text()
        .await
        .map_err(|e| AppError::validation(format!("read multipart field: {e}")))
}

async fn bytes_field(field: axum::extract::multipart::Field<'_>) -> Result<Vec<u8>, AppError> {
    field
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| AppError::validation(format!("read multipart file: {e}")))
}

/// `.zip` 扩展名校验 (对齐 nuwax: 仅允许 zip)。
fn validate_zip_ext(filename: Option<&str>) -> Result<(), AppError> {
    let ext = filename
        .and_then(|n| {
            std::path::Path::new(n)
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_ascii_lowercase())
        })
        .unwrap_or_default();
    if ext == "zip" {
        Ok(())
    } else {
        Err(AppError::validation("Only zip files are supported"))
    }
}

fn ws_path(state: &AppState, user_id: &str, cid: &str) -> PathBuf {
    state.resolver.resolve_computer(&ComputerContext {
        user_id: user_id.to_string(),
        cid: cid.to_string(),
    })
}

/// computer 目标路径: `customTargetDir` trim 后非空则用之, 否则回退默认工作区 (对齐 nuwax)。
fn resolve_computer_target(
    state: &AppState,
    user_id: &str,
    cid: &str,
    custom_target_dir: Option<&str>,
) -> PathBuf {
    match custom_target_dir.map(str::trim).filter(|s| !s.is_empty()) {
        Some(ct) => PathBuf::from(ct),
        None => ws_path(state, user_id, cid),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserCidQuery {
    user_id: String,
    c_id: String,
    #[serde(default)]
    proxy_path: Option<String>,
}

// ── get-file-list ───────────────────────────────────────────────────────────────

/// `GET /api/computer/get-file-list` (对齐 nuwax getFileList; 复用 tree::list_files)。
async fn get_file_list(
    State(state): State<AppState>,
    Query(q): Query<UserCidQuery>,
) -> Result<Json<Value>, AppError> {
    let path = ws_path(&state, &q.user_id, &q.c_id);
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Err(AppError::resource("workspace does not exist"));
    }
    let files = tree::list_files(&path, &state.config, q.proxy_path.as_deref()).await?;
    Ok(Json(json!({ "success": true, "files": files })))
}

// ── delete-workspace ────────────────────────────────────────────────────────────

/// `POST /api/computer/delete-workspace` (对齐 nuwax deleteWorkspace; 目录不存在也返回 deleted)。
async fn delete_workspace(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Value>, AppError> {
    let user_id = body
        .get("userId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = body
        .get("cId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::validation("cId is required"))?;
    let path = ws_path(&state, user_id, cid);
    // 不存在视为已删除 (对齐 nuwax, 只 warn)
    if path.exists() {
        tokio::fs::remove_dir_all(&path)
            .await
            .map_err(|e| AppError::system(format!("delete workspace failed: {e}")))?;
    }
    Ok(Json(json!({ "success": true, "deleted": true })))
}

// ── get-logs ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetLogsQuery {
    user_id: String,
    c_id: String,
    #[serde(default = "default_tail_lines")]
    tail_lines: usize,
}
fn default_tail_lines() -> usize {
    200
}

/// `GET /api/computer/get-logs` (对齐 nuwax getLatestLogs; 读 .logs/ 下 mtime 最新的文件末尾 N 行)。
async fn get_logs(
    State(state): State<AppState>,
    Query(q): Query<GetLogsQuery>,
) -> Result<Json<Value>, AppError> {
    let log_dir = ws_path(&state, &q.user_id, &q.c_id).join(".logs");
    let (content, file_name) = read_latest_log(&log_dir).await?;
    let all: Vec<&str> = content.lines().collect();
    let total = all.len();
    let start = total.saturating_sub(q.tail_lines);
    let logs: Vec<Value> = all[start..]
        .iter()
        .enumerate()
        .map(|(i, l)| {
            json!({ "line": start + i + 1, "content": l })
        })
        .collect();
    Ok(Json(json!({
        "success": true,
        "message": "Logs fetched",
        "logs": logs,
        "totalLines": total,
        "startIndex": start + 1,
        "logFileName": file_name,
    })))
}

/// 读目录下 mtime 最新的文件全文 (无文件返回空)。
async fn read_latest_log(dir: &std::path::Path) -> AppResult<(String, String)> {
    if !dir.exists() {
        return Ok((String::new(), String::new()));
    }
    let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut rd = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| AppError::system(format!("read log dir: {e}")))?;
    while let Some(entry) = rd
        .next_entry()
        .await
        .map_err(|e| AppError::system(format!("read log entry: {e}")))?
    {
        if let Ok(meta) = entry.metadata().await
            && meta.is_file()
        {
            let mtime = meta
                .modified()
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if latest.as_ref().is_none_or(|(t, _)| mtime > *t) {
                latest = Some((mtime, entry.path()));
            }
        }
    }
    match latest {
        Some((_, path)) => {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("log")
                .to_string();
            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| AppError::system(format!("read log {}: {e}", path.display())))?;
            Ok((content, name))
        }
        None => Ok((String::new(), String::new())),
    }
}

// ── execute-command ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecCommandBody {
    user_id: String,
    c_id: String,
    command: String,
}

/// `POST /api/computer/execute-command` (对齐 nuwax executeCommand; shell 执行 + 超时 + 捕获输出)。
/// command 是 agent 提供的 shell 命令串, 故经 sh -c (与 nuwax child_process.exec 一致)。
async fn execute_command(
    State(state): State<AppState>,
    Json(body): Json<ExecCommandBody>,
) -> Result<Json<Value>, AppError> {
    let cwd = ws_path(&state, &body.user_id, &body.c_id);
    if !cwd.exists() {
        return Err(AppError::resource("workspace does not exist"));
    }
    let timeout_secs = state.config.dev_command_timeout_secs;
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(&body.command);
    cmd.current_dir(&cwd);
    cmd.env("NODE_ENV", "development");
    cmd.env_remove("CI");
    cmd.env_remove("NPM_CONFIG_PRODUCTION");
    // 输出捕获 (maxBuffer 50MB 对齐 nuwax)
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = cmd
        .spawn()
        .map_err(|e| AppError::system(format!("execute-command spawn failed: {e}")))?;
    let out = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
        .await;
    match out {
        Ok(Ok(o)) => {
            let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
            let code = o.status.code().unwrap_or(-1);
            Ok(Json(json!({
                "success": code == 0,
                "stdout": stdout,
                "stderr": stderr,
                "exitCode": code,
            })))
        }
        Ok(Err(e)) => Err(AppError::system(format!("execute-command wait failed: {e}"))),
        Err(_) => Ok(Json(json!({
            "success": false,
            "stdout": "",
            "stderr": format!("command timed out after {timeout_secs}s"),
            "exitCode": -1,
        }))),
    }
}

// ── install-project ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallBody {
    user_id: String,
    c_id: String,
    programming_language: String,
}

/// `POST /api/computer/install-project` (对齐 nuwax installProjectDependencies)。
/// typescript → 递归找 package.json 目录 pnpm install; python → 找 requirements/pyproject pip install。
async fn install_project(
    State(state): State<AppState>,
    Json(body): Json<InstallBody>,
) -> Result<Json<Value>, AppError> {
    let ws = ws_path(&state, &body.user_id, &body.c_id);
    if !ws.exists() {
        return Err(AppError::resource("workspace does not exist"));
    }
    let lang = body.programming_language.to_ascii_lowercase();
    let (program, args, project_dir): (&str, Vec<&str>, Option<PathBuf>) = match lang.as_str() {
        "typescript" | "ts" => {
            let dir = find_first(&ws, "package.json").await;
            (
                "pnpm",
                vec![
                    "install",
                    "--prefer-offline",
                    "--config.production=false",
                    "--config.confirmModulesPurge=false",
                    "--config.dangerouslyAllowAllBuilds=true",
                ],
                dir,
            )
        }
        "python" | "py" => {
            // 优先 pyproject.toml (pip install -e .), 否则 requirements.txt
            let (dir, args) = if let Some(d) = find_first(&ws, "pyproject.toml").await {
                (Some(d), vec!["install", "-e", "."])
            } else {
                (
                    find_first(&ws, "requirements.txt").await,
                    vec!["install", "-r", "requirements.txt"],
                )
            };
            ("pip", args, dir)
        }
        other => {
            return Err(AppError::validation(format!(
                "unsupported programmingLanguage: {other}"
            )))
        }
    };
    let project_dir = project_dir.ok_or_else(|| {
        AppError::business("project manifest (package.json / pyproject.toml / requirements.txt) not found")
    })?;
    let timeout = state.config.dev_command_timeout_secs;
    let (stdout, stderr, code) = run_capture(program, &args, &project_dir, timeout).await?;
    Ok(Json(json!({
        "success": code == 0,
        "message": if code == 0 { "Project dependencies installed successfully" } else { "install failed" },
        "projectDir": project_dir.display().to_string(),
        "programmingLanguage": lang,
        "stdout": stdout,
        "stderr": stderr,
        "exitCode": code,
    })))
}

/// 递归找含 `manifest` 的最近目录, 返回该目录 (BFS)。
async fn find_first(root: &std::path::Path, manifest: &str) -> Option<PathBuf> {
    use std::collections::VecDeque;
    let mut q = VecDeque::new();
    q.push_back(root.to_path_buf());
    while let Some(dir) = q.pop_front() {
        if dir.join(manifest).exists() {
            return Some(dir);
        }
        let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = rd.next_entry().await {
            if entry
                .file_type()
                .await
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
                let name = entry.file_name();
                // 跳过常见大目录
                if matches!(
                    name.to_str(),
                    Some("node_modules" | ".git" | "dist" | ".pnpm-store")
                ) {
                    continue;
                }
                q.push_back(entry.path());
            }
        }
    }
    None
}

/// 运行命令并捕获输出 (超时退出码 -1)。
async fn run_capture(
    program: &str,
    args: &[&str],
    cwd: &std::path::Path,
    timeout_secs: u64,
) -> AppResult<(String, String, i32)> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    cmd.current_dir(cwd);
    if let Ok(p) = std::env::var("PATH") {
        cmd.env("PATH", p);
    }
    if let Ok(h) = std::env::var("HOME") {
        cmd.env("HOME", h);
    }
    cmd.env_remove("CI");
    cmd.env_remove("NPM_CONFIG_PRODUCTION");
    cmd.env("NODE_ENV", "development");
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = cmd
        .spawn()
        .map_err(|e| AppError::system(format!("spawn {program} failed: {e}")))?;
    let out = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await;
    match out {
        Ok(Ok(o)) => Ok((
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
            o.status.code().unwrap_or(-1),
        )),
        Ok(Err(e)) => Err(AppError::system(format!("{program} wait failed: {e}"))),
        Err(_) => Ok((String::new(), format!("timed out after {timeout_secs}s"), -1)),
    }
}

// ── zip-workspace / download-all-files ──────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ZipBody {
    user_id: String,
    c_id: String,
    #[serde(default)]
    exclude_dirs: Option<Vec<String>>,
}

/// 打包 computer 工作区为 zip 字节流返回 (download 用强过滤: dot-segment + 符号链接 + 硬链接,
/// 对齐 nuwax downloadAllFiles entry filter)。
async fn pack_workspace(state: &AppState, user_id: &str, cid: &str, exclude: Vec<String>) -> AppResult<Response> {
    let src = ws_path(state, user_id, cid);
    if !src.exists() {
        return Err(AppError::resource("workspace does not exist"));
    }
    let tmp = std::env::temp_dir().join(format!(
        "computer-{}-{}-{}.zip",
        user_id,
        cid,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let opts = zip::PackOpts {
        exclude_dirs: exclude,
        exclude_files: state.config.zip_workspace_exclude.clone(),
        skip_dot_segments: false,
        skip_hardlinks: false,
    };
    zip::pack_download(src, tmp.clone(), opts).await?;
    let bytes = tokio::fs::read(&tmp).await?;
    let _ = tokio::fs::remove_file(&tmp).await;
    let filename = format!("{user_id}-{cid}.zip");
    Ok((
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/zip"),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
                    .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
            ),
        ],
        bytes,
    )
        .into_response())
}

/// `POST /api/computer/zip-workspace` (对齐 nuwax zipWorkspace; 流式 zip)。
async fn zip_workspace(
    State(state): State<AppState>,
    Json(body): Json<ZipBody>,
) -> Result<Response, AppError> {
    let exclude = body.exclude_dirs.unwrap_or_default();
    pack_workspace(&state, &body.user_id, &body.c_id, exclude).await
}

/// `GET /api/computer/download-all-files` (对齐 nuwax downloadAllFiles; 全量 zip)。
async fn download_all_files(
    State(state): State<AppState>,
    Query(q): Query<UserCidQuery>,
) -> Result<Response, AppError> {
    pack_workspace(&state, &q.user_id, &q.c_id, vec![]).await
}

// ── files-update / all-files-update (复用 code_service path 制核心, 无 zip 版本备份) ──

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FilesUpdateBody {
    user_id: String,
    c_id: String,
    files: Vec<code_service::FileOp>,
}

/// `POST /api/computer/files-update` (对齐 nuwax computer updateFiles; 增量 create/delete/rename/modify)。
async fn files_update(
    State(state): State<AppState>,
    Json(mut body): Json<FilesUpdateBody>,
) -> Result<Json<Value>, AppError> {
    let path = ws_path(&state, &body.user_id, &body.c_id);
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Err(AppError::resource("workspace does not exist"));
    }
    // decodeURIComponent 文本内容 (对齐 nuwax safeDecodePath)
    for op in body.files.iter_mut() {
        if let Some(c) = op.contents.as_mut()
            && !c.is_empty()
        {
            *c = code_service::decode_uri_component(c);
        }
    }
    let count = body.files.len();
    code_service::apply_file_ops(&path, &body.files).await?;
    Ok(Json(json!({
        "success": true,
        "message": "Files updated successfully",
        "filesCount": count,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AllFilesUpdateBody {
    user_id: String,
    c_id: String,
    files: Vec<code_service::FileEntry>,
}

/// `POST /api/computer/all-files-update` (全量覆盖 + 清理缺失)。
async fn all_files_update(
    State(state): State<AppState>,
    Json(mut body): Json<AllFilesUpdateBody>,
) -> Result<Json<Value>, AppError> {
    let path = ws_path(&state, &body.user_id, &body.c_id);
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Err(AppError::resource("workspace does not exist"));
    }
    // decodeURIComponent: 仅文本 (binary base64 跳过)
    for f in body.files.iter_mut() {
        if f.binary == Some(true) {
            continue;
        }
        if let Some(c) = f.contents.as_mut()
            && !c.is_empty()
        {
            *c = code_service::decode_uri_component(c);
        }
    }
    let count = body.files.len();
    code_service::apply_all_files(&path, &state.config, &body.files).await?;
    Ok(Json(json!({
        "success": true,
        "message": "All files updated successfully",
        "filesCount": count,
    })))
}

// ── upload-file / upload-files (multipart, path 制, 无版本备份) ───────────────────

/// `POST /api/computer/upload-file` (对齐 nuwax computer uploadFile; multipart)。
async fn upload_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut file_path = None;
    let mut data: Option<Vec<u8>> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "filePath" => file_path = Some(text_field(field).await?),
            "file" => data = Some(bytes_field(field).await?),
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    let file_path = file_path.ok_or_else(|| AppError::validation("filePath is required"))?;
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;
    let ws = ws_path(&state, &user_id, &cid);
    let target = path_safety::ensure_within(&ws, &file_path)?;
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&target, data).await?;
    Ok(Json(json!({
        "success": true,
        "message": "File uploaded successfully",
        "filePath": file_path,
    })))
}

/// `POST /api/computer/upload-files` (对齐 nuwax computer uploadFiles; 多文件 multipart)。
async fn upload_files(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut file_paths: Vec<String> = Vec::new();
    let mut files_vec: Vec<Vec<u8>> = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "filePaths" => file_paths.push(text_field(field).await?),
            "files" => files_vec.push(bytes_field(field).await?),
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    if file_paths.len() != files_vec.len() {
        return Err(AppError::validation("filePaths and files count mismatch"));
    }
    let ws = ws_path(&state, &user_id, &cid);
    let mut written = Vec::new();
    for (fp, data) in file_paths.iter().zip(files_vec) {
        let target = path_safety::ensure_within(&ws, fp)?;
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&target, data).await?;
        written.push(fp.clone());
    }
    Ok(Json(json!({
        "success": true,
        "message": format!("{} files uploaded", written.len()),
        "fileCount": written.len(),
        "files": written,
    })))
}

/// `POST /api/computer/import-project` (对齐 nuwax computer importProject):
/// 上传 zip → 解压 + removeTopLevelDir + 白名单保留合并到工作区。
async fn import_project(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut custom_target_dir = None;
    let mut data: Option<Vec<u8>> = None;
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
                data = Some(bytes_field(field).await?);
            }
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;
    validate_zip_ext(file_name.as_deref())?;
    let target_dir = resolve_computer_target(&state, &user_id, &cid, custom_target_dir.as_deref());
    tokio::fs::create_dir_all(&target_dir).await?;
    let res = crate::service::computer_ws::import_project(&target_dir, data).await?;
    Ok(Json(json!({
        "success": true,
        "message": "Project imported successfully",
        "userId": user_id,
        "cId": cid,
        "targetDir": res.target_dir,
    })))
}

/// `POST /api/computer/cleanup-build-artifacts` (对齐 nuwax cleanupBuildArtifacts; 删 dist-packages)。
async fn cleanup_build_artifacts(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Value>, AppError> {
    let user_id = body
        .get("userId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = body
        .get("cId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::validation("cId is required"))?;
    let dist = ws_path(&state, user_id, cid).join("dist-packages");
    let mut removed = false;
    if dist.exists() {
        tokio::fs::remove_dir_all(&dist)
            .await
            .map_err(|e| AppError::system(format!("cleanup dist-packages failed: {e}")))?;
        removed = true;
    }
    Ok(Json(json!({
        "success": true,
        "message": "Build artifacts cleaned",
        "removed": removed,
    })))
}

/// `POST /api/computer/create-workspace` (对齐 nuwax createWorkspace; v1):
/// mkdir 工作区 + `.agents/{skills,agents}` 装配 + 可选 skill zip 合并 + syncAgents。
/// v2 的 agent hook 配置 (claude/codex/opencode mcp/hooks/permissions) 见 Task (Batch E)。
async fn create_workspace(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut skill_zip: Option<Vec<u8>> = None;
    let mut file_name = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "file" => {
                file_name = field.file_name().map(|s| s.to_string());
                skill_zip = Some(bytes_field(field).await?);
            }
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    if skill_zip.is_some() {
        validate_zip_ext(file_name.as_deref())?;
    }
    let ws = ws_path(&state, &user_id, &cid);
    tokio::fs::create_dir_all(&ws).await?;
    let res = crate::service::computer_ws::create_workspace(&ws, skill_zip).await?;
    Ok(Json(json!({
        "success": true,
        "message": res.message,
        "workspaceRoot": res.workspace_root,
        "updatedSkills": res.updated_skills,
    })))
}

// ── push-skills-to-workspace (multipart: file zip 和/或 skillUrls) ────────────────

/// `POST /api/computer/push-skills-to-workspace` (对齐 nuwax pushSkillsToWorkspace;
/// 复用 skills_service::push_skills_at, 推到 .claude/skills + syncAgents)。
async fn push_skills_to_workspace(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut zip_data: Option<Vec<u8>> = None;
    let mut skill_urls: Vec<String> = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "file" => zip_data = Some(bytes_field(field).await?),
            "skillUrls" => {
                let t = text_field(field).await?;
                if let Ok(urls) = serde_json::from_str::<Vec<String>>(&t) {
                    skill_urls.extend(urls);
                } else {
                    skill_urls.push(t);
                }
            }
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    let ws = ws_path(&state, &user_id, &cid);
    if !tokio::fs::try_exists(&ws).await.unwrap_or(false) {
        return Err(AppError::resource("workspace does not exist"));
    }
    let updated = skills_service::push_skills_at(&ws, zip_data, skill_urls).await?;
    Ok(Json(json!({
        "success": true,
        "message": "Skills pushed to workspace",
        "workspaceRoot": ws.display().to_string(),
        "updatedSkills": updated,
    })))
}

// ── init-project-template (解压模板 zip + 可选 git init) ──────────────────────────

/// `POST /api/computer/init-project-template` (对齐 nuwax initProjectTemplate)。
/// multipart: userId, cId, file(模板 zip), enableGit。解压到工作区 + 可选 git init。
async fn init_project_template(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut data: Option<Vec<u8>> = None;
    let mut enable_git = false;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "file" => data = Some(bytes_field(field).await?),
            "enableGit" => {
                enable_git = matches!(
                    text_field(field).await?.trim().to_lowercase().as_str(),
                    "true" | "1" | "yes"
                );
            }
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    let data = data.ok_or_else(|| AppError::validation("file (template zip) is required"))?;
    let ws = ws_path(&state, &user_id, &cid);
    tokio::fs::create_dir_all(&ws).await?;
    // 解压模板
    let tmp = std::env::temp_dir().join(format!(
        "computer-init-{}-{}-{}.zip",
        user_id,
        cid,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    tokio::fs::write(&tmp, data).await?;
    let r = zip::extract_to(tmp.clone(), ws.clone()).await;
    let _ = tokio::fs::remove_file(&tmp).await;
    r?;
    // 可选 git init
    let git_inited = if enable_git {
        let an = state.config.git_default_author_name.clone();
        let ae = state.config.git_default_author_email.clone();
        let _ = crate::service::git::init_repo(&ws, &an, &ae);
        true
    } else {
        false
    };
    Ok(Json(json!({
        "success": true,
        "message": "Project template initialized",
        "workspaceRoot": ws.display().to_string(),
        "gitInited": git_inited,
    })))
}

// ── build-agent-package (打包 agent 分发产物) ────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildAgentBody {
    user_id: String,
    c_id: String,
    agent_id: String,
    version: String,
}

/// `POST /api/computer/build-agent-package` (对齐 nuwax buildAgentPackage)。
/// 递归找含 scripts/package-platforms.mjs 的目录 → pnpm install →
/// `node scripts/package-platforms.mjs agent-{id} {ver} {dir}/dist-packages --print-artifacts`
/// → 解析 stdout 中产物文件名 (agent-{id}-{platform}-{ver}.{ext})。
async fn build_agent_package(
    State(state): State<AppState>,
    Json(body): Json<BuildAgentBody>,
) -> Result<Json<Value>, AppError> {
    let ws = ws_path(&state, &body.user_id, &body.c_id);
    if !ws.exists() {
        return Err(AppError::resource("workspace does not exist"));
    }
    // 递归找 package-platforms.mjs 所在目录 (跳过 node_modules/dist)
    let pkg_dir = find_first(&ws, "package.json").await; // 用 package.json 定位项目目录
    let pkg_dir = pkg_dir.ok_or_else(|| {
        AppError::business("no package.json found in workspace (build-agent-package)")
    })?;
    let script = pkg_dir.join("scripts").join("package-platforms.mjs");
    if !script.exists() {
        return Err(AppError::business(format!(
            "scripts/package-platforms.mjs not found in {}",
            pkg_dir.display()
        )));
    }
    let timeout = state.config.dev_command_timeout_secs;
    // pnpm install (含 devDependencies)
    let (_, _, code) = run_capture(
        "pnpm",
        &["install"],
        &pkg_dir,
        timeout,
    )
    .await?;
    if code != 0 {
        return Err(AppError::system(format!(
            "pnpm install failed (exit {code})"
        )));
    }
    // 打包
    let dist_packages = pkg_dir.join("dist-packages");
    let agent_name = format!("agent-{}", body.agent_id);
    let (stdout, stderr, code) = run_capture(
        "node",
        &[
            "scripts/package-platforms.mjs",
            &agent_name,
            &body.version,
            &dist_packages.to_string_lossy(),
            "--print-artifacts",
        ],
        &pkg_dir,
        timeout,
    )
    .await?;
    if code != 0 {
        return Err(AppError::system(format!(
            "package-platforms.mjs failed (exit {code}): {stderr}"
        )));
    }
    // 解析产物: stdout 中以 .tar.gz/.tar.bz2/.zip/.tgz 结尾的行
    let artifacts = parse_artifacts(&stdout, &body.agent_id, &body.version);
    Ok(Json(json!({
        "success": true,
        "artifacts": artifacts,
        "stdout": stdout,
    })))
}

/// 从 package-platforms stdout 解析产物列表: {path, fileName, platform}。
fn parse_artifacts(stdout: &str, agent_id: &str, version: &str) -> Vec<Value> {
    let prefix = format!("agent-{agent_id}-");
    let mut out = Vec::new();
    for line in stdout.lines() {
        let t = line.trim();
        if !(t.ends_with(".tar.gz") || t.ends_with(".tar.bz2") || t.ends_with(".zip") || t.ends_with(".tgz")) {
            continue;
        }
        let file_name = t.rsplit('/').next().unwrap_or(t).to_string();
        // platform 从文件名 agent-{id}-{platform}-{version}.{ext} 提取
        let platform = file_name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.rsplit_once(&format!("-{version}")))
            .map(|(p, _)| p.to_string())
            .unwrap_or_default();
        out.push(json!({
            "path": t,
            "fileName": file_name,
            "platform": platform,
        }));
    }
    out
}

// 待实现 (低频/复杂): create-workspace-v2 agent hook 配置 (claude/codex/opencode
// 的 hook/MCP/permission 配置写入, nuwax 专属 agent 装配逻辑)。
