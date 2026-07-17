//! `/api/computer` 路由 (对齐 nuwax computerRoutes)。
//!
//! computer 工作区路径: `{COMPUTER_WORKSPACE_ROOT}/{userId}/{cId}/`。
//! 本文件含 6 个可直接实现的路由 (路径制/简单 spawn); files-update / upload /
//! create-workspace / build-agent-package 等依赖 resolver+ctx 改造的路由见后续。

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::{Multipart, Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{AppError, AppResult};
use crate::path_safety;
use crate::service::{code as code_service, pnpm_config, skills as skills_service, tree, zip};
use crate::workspace::ComputerContext;

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
        .route("/upload-file", post(upload_file))
        .route("/upload-files", post(upload_files))
        .route("/import-project", post(import_project))
        .route("/cleanup-build-artifacts", post(cleanup_build_artifacts))
        .route("/create-workspace", post(create_workspace))
        .route("/create-workspace-v2", post(create_workspace_v2))
        .route("/push-skills-to-workspace", post(push_skills_to_workspace))
        .route(
            "/push-skills-to-workspace-v2",
            post(push_skills_to_workspace),
        )
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
            Path::new(n)
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
    #[serde(default)]
    custom_target_dir: Option<String>,
}

// ── get-file-list ───────────────────────────────────────────────────────────────

/// `GET /api/computer/get-file-list` (对齐 nuwax getFileList):
/// 轻量元信息遍历 (不读内容) + customTargetDir 覆盖; 目录不存在返回空数组。
async fn get_file_list(
    State(state): State<AppState>,
    Query(q): Query<UserCidQuery>,
) -> Result<Json<Value>, AppError> {
    let path = resolve_computer_target(&state, &q.user_id, &q.c_id, q.custom_target_dir.as_deref());
    // 对齐 nuwax: 目录不存在 → 返回空数组 (非报错)
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Ok(Json(json!({ "success": true, "files": [] })));
    }
    let mut files = tree::list_files_meta(&path, &state.config, q.proxy_path.as_deref()).await?;
    // fileProxyUrl 追加 ?customTargetDir (对齐 nuwax; 值需 encodeURIComponent)
    if let Some(ct) = q
        .custom_target_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let suffix = format!(
            "?customTargetDir={}",
            code_service::encode_uri_component(ct)
        );
        for f in files.iter_mut() {
            if let Some(u) = f.file_proxy_url.as_mut() {
                u.push_str(&suffix);
            }
        }
    }
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
/// 空场景区分 message, logFileName=null; 成功 message="Get log successfully"; 过滤空行。
async fn get_logs(
    State(state): State<AppState>,
    Query(q): Query<GetLogsQuery>,
) -> Result<Json<Value>, AppError> {
    let log_dir = resolve_computer_target(&state, &q.user_id, &q.c_id, None).join(".logs");
    let empty_resp = |msg: &str| {
        Json(json!({
            "success": true,
            "message": msg,
            "logs": [],
            "totalLines": 0,
            "startIndex": 1,
            "logFileName": null,
        }))
    };
    if !log_dir.exists() {
        return Ok(empty_resp("Log directory does not exist"));
    }
    let latest = match latest_log_file(&log_dir).await? {
        Some(p) => p,
        None => return Ok(empty_resp("No log file found")),
    };
    let name = latest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("log")
        .to_string();
    let content = tokio::fs::read_to_string(&latest)
        .await
        .map_err(|e| AppError::system(format!("read log {}: {e}", latest.display())))?;
    // 过滤空行 (对齐 nuwax filter(l => l.length > 0))
    let all: Vec<&str> = content.split('\n').filter(|l| !l.is_empty()).collect();
    let total = all.len();
    let start = total.saturating_sub(q.tail_lines);
    let logs: Vec<Value> = all[start..]
        .iter()
        .enumerate()
        .map(|(i, l)| json!({ "line": start + i + 1, "content": l }))
        .collect();
    Ok(Json(json!({
        "success": true,
        "message": "Get log successfully",
        "logs": logs,
        "totalLines": total,
        "startIndex": start + 1,
        "logFileName": name,
    })))
}

/// 读目录下 mtime 最新的文件路径 (无文件返回 None)。
async fn latest_log_file(dir: &Path) -> AppResult<Option<PathBuf>> {
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
            let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if latest.as_ref().is_none_or(|(t, _)| mtime > *t) {
                latest = Some((mtime, entry.path()));
            }
        }
    }
    Ok(latest.map(|(_, p)| p))
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
/// command 是 agent 提供的 shell 命令串, 故经 shell -c (与 nuwax child_process.exec 一致)。
/// shell 优先用 `BASH_PATH` (未配置则 sh); stdout/stderr 截断到 50MB (对齐 nuwax maxBuffer)。
async fn execute_command(
    State(state): State<AppState>,
    Json(body): Json<ExecCommandBody>,
) -> Result<Json<Value>, AppError> {
    let cwd = ws_path(&state, &body.user_id, &body.c_id);
    if !cwd.exists() {
        return Err(AppError::resource("workspace does not exist"));
    }
    if body.command.trim().is_empty() {
        return Err(AppError::validation("command cannot be empty"));
    }
    let timeout_secs = state.config.dev_command_timeout_secs;
    // shell: BASH_PATH 非空则用之, 否则 sh (对齐 nuwax execOptions.shell)
    let shell = {
        let b = state.config.bash_path.trim();
        if b.is_empty() { "sh" } else { b }
    };
    let mut cmd = tokio::process::Command::new(shell);
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
    let out =
        tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await;
    match out {
        Ok(Ok(o)) => {
            let stdout = cap_50mb(String::from_utf8_lossy(&o.stdout));
            let stderr = cap_50mb(String::from_utf8_lossy(&o.stderr));
            let code = o.status.code().unwrap_or(-1);
            Ok(Json(json!({
                "success": code == 0,
                "stdout": stdout,
                "stderr": stderr,
                "exitCode": code,
            })))
        }
        Ok(Err(e)) => Err(AppError::system(format!(
            "execute-command wait failed: {e}"
        ))),
        Err(_) => Ok(Json(json!({
            "success": false,
            "stdout": "",
            "stderr": format!("command timed out after {timeout_secs}s"),
            "exitCode": -1,
        }))),
    }
}

/// 输出截断到 50MB (对齐 nuwax exec maxBuffer 50 * 1024 * 1024), 防止超大输出 OOM。
fn cap_50mb(cow: std::borrow::Cow<'_, str>) -> String {
    const MAX: usize = 50 * 1024 * 1024;
    let s = cow.into_owned();
    if s.len() <= MAX {
        return s;
    }
    // 按字节边界截断 (MAX 处可能落在多字节字符中间, 取 char_boundary 安全边界)
    let mut end = MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
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
    let skip = package_search_skip_dirs(&state.config.zip_workspace_exclude);
    let (program, args, project_dir): (&str, Vec<&str>, Option<PathBuf>) = match lang.as_str() {
        "typescript" | "ts" => {
            // projectDir = findPackageScript || findNodeProjectDir (对齐 nuwax)
            let dir = match find_package_script(&ws, &skip).await {
                Some(d) => Some(d),
                None => find_first(&ws, "package.json").await,
            };
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
            )));
        }
    };
    let project_dir = project_dir.ok_or_else(|| {
        AppError::business(
            "project manifest (package.json / pyproject.toml / requirements.txt) not found",
        )
    })?;
    let timeout = state.config.dev_command_timeout_secs;
    // pnpm install 前准备 .npmrc (package-import-method=copy + built-deps sanitize + 3 行),
    // best-effort (失败仅 warn, 不阻断 install; 对齐 nuwax ensurePnpmInstallConfig)
    if program == "pnpm" {
        pnpm_config::ensure_pnpm_install_config(&project_dir).await;
    }
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
async fn find_first(root: &Path, manifest: &str) -> Option<PathBuf> {
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
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
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

/// build-agent-package / cleanup / install 定位项目目录时的搜索跳过集合
/// (对齐 nuwax PACKAGE_SEARCH_SKIP_DIRS = ZIP_WORKSPACE_EXCLUDE ∪ {dist-packages})。
fn package_search_skip_dirs(zip_workspace_exclude: &[String]) -> Vec<String> {
    let mut v = zip_workspace_exclude.to_vec();
    if !v.iter().any(|d| d == "dist-packages") {
        v.push("dist-packages".to_string());
    }
    v
}

/// 递归查找含 `scripts/package-platforms.mjs` 的目录 (对齐 nuwax findPackageScript)。
/// 深度优先, 跳过 skip_dirs 命中的目录名。
async fn find_package_script(root: &Path, skip_dirs: &[String]) -> Option<PathBuf> {
    if root.join("scripts").join("package-platforms.mjs").exists() {
        return Some(root.to_path_buf());
    }
    let Ok(mut rd) = tokio::fs::read_dir(root).await else {
        return None;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let Ok(ft) = entry.file_type().await else {
            continue;
        };
        if !ft.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if skip_dirs.iter().any(|d| d == &name) {
            continue;
        }
        if let Some(found) = Box::pin(find_package_script(&entry.path(), skip_dirs)).await {
            return Some(found);
        }
    }
    None
}

/// 运行命令并捕获输出 (超时退出码 -1)。
async fn run_capture(
    program: &str,
    args: &[&str],
    cwd: &Path,
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
    let out =
        tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await;
    match out {
        Ok(Ok(o)) => Ok((
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
            o.status.code().unwrap_or(-1),
        )),
        Ok(Err(e)) => Err(AppError::system(format!("{program} wait failed: {e}"))),
        Err(_) => Ok((
            String::new(),
            format!("timed out after {timeout_secs}s"),
            -1,
        )),
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

/// 临时 zip 路径 (基于 user/cid + 纳秒, 避免并发冲突)。
fn computer_tmp_zip(user_id: &str, cid: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("computer-{user_id}-{cid}-{nanos}.zip"))
}

/// zip 下载响应头: Content-Type + UTF-8 Content-Disposition (对齐 nuwax `filename` + `filename*`)。
fn zip_response(filename: &str, bytes: Vec<u8>) -> Response {
    let disposition = format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        filename,
        utf8_percent_encode(filename)
    );
    (
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/zip"),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&disposition)
                    .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
            ),
        ],
        bytes,
    )
        .into_response()
}

/// 文件名 RFC 5987 百分号编码 (仅 [A-Za-z0-9-._~] 不编码)。
fn utf8_percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `POST /api/computer/zip-workspace` (对齐 nuwax zipWorkspace):
/// 无顶层前缀; 工作区不存在则报错; 文件名 `${userId}_${cId}.zip` + UTF-8 filename*。
/// 过滤: ZIP_WORKSPACE_EXCLUDE (强制) + 调用方 excludeDirs (补充) 合并, 对任意路径段匹配
/// (目录与文件同集合); 跳过符号链接; **无** dot-segment 过滤 (保留 .gitignore/.npmrc 等)。
async fn zip_workspace(
    State(state): State<AppState>,
    Json(body): Json<ZipBody>,
) -> Result<Response, AppError> {
    let src = ws_path(&state, &body.user_id, &body.c_id);
    if !src.exists() {
        return Err(AppError::resource("workspace does not exist"));
    }
    let tmp = computer_tmp_zip(&body.user_id, &body.c_id);
    // mandatory(ZIP_WORKSPACE_EXCLUDE) ∪ extra(调用方 excludeDirs); 同时填 dirs 与 files,
    // 等价 nuwax archive.directory 的 "任一路径段命中集合则跳过" (对目录与文件同集合)。
    let merged: Vec<String> = state
        .config
        .zip_workspace_exclude
        .iter()
        .cloned()
        .chain(body.exclude_dirs.unwrap_or_default())
        .collect();
    let opts = zip::PackOpts {
        exclude_dirs: merged.clone(),
        exclude_files: merged,
        // 不用 pack_download: 关闭 dot-segment/hardlink 过滤 (nuwax zipWorkspace 保留 .gitignore)
        skip_dot_segments: false,
        skip_hardlinks: false,
        path_prefix: None,
    };
    zip::pack_with_opts(src, tmp.clone(), opts).await?;
    let bytes = tokio::fs::read(&tmp).await?;
    let _ = tokio::fs::remove_file(&tmp).await;
    let filename = format!("{}_{}.zip", body.user_id, body.c_id);
    Ok(zip_response(&filename, bytes))
}

/// `GET /api/computer/download-all-files` (对齐 nuwax downloadAllFiles):
/// 顶层前缀 `${userId}_${cId}/` + 空 zip 兜底 + 100MB 大小上限 + UTF-8 filename* + customTargetDir。
async fn download_all_files(
    State(state): State<AppState>,
    Query(q): Query<UserCidQuery>,
) -> Result<Response, AppError> {
    let src = resolve_computer_target(&state, &q.user_id, &q.c_id, q.custom_target_dir.as_deref());
    let prefix = format!("{}_{}/", q.user_id, q.c_id);
    let filename = format!("{}_{}.zip", q.user_id, q.c_id);
    let tmp = computer_tmp_zip(&q.user_id, &q.c_id);

    // 工作区不存在 → 空 zip 兜底 (仅含顶层目录条目, 对齐 nuwax)
    if !src.exists() {
        zip::write_empty_zip(tmp.clone(), prefix.clone()).await?;
        let bytes = tokio::fs::read(&tmp).await?;
        let _ = tokio::fs::remove_file(&tmp).await;
        return Ok(zip_response(&filename, bytes));
    }

    // 过滤对齐 nuwax downloadAllFiles: excludeDirs=TRAVERSE_EXCLUDE_DIRS,
    // excludeFiles=CONTENT_TRAVERSE_EXCLUDE_FILES, 叠加 pack_download 的 dot-segment +
    // 符号链接/硬链接过滤。**非** zip_workspace_exclude (后者当文件名匹配, 目录如
    // node_modules/dist 不被排除 → zip 爆体积)。
    let opts = zip::PackOpts {
        exclude_dirs: state.config.traverse_exclude_dirs.clone(),
        exclude_files: state.config.content_traverse_exclude_files.clone(),
        path_prefix: Some(prefix),
        ..Default::default()
    };
    // 大小上限 (对齐 nuwax DOWNLOAD_MAX_FILE_SIZE_BYTES, 默认 100MB)
    let max = state.config.download_max_file_size_bytes;
    let src_for_size = src.clone();
    let opts_for_size = opts.clone();
    let size = tokio::task::spawn_blocking(move || {
        zip::downloadable_size_blocking(&src_for_size, &opts_for_size)
    })
    .await
    .map_err(|e| AppError::system(format!("size calc join: {e}")))?;
    if size > max {
        let cur_mb = size / 1024 / 1024;
        let max_mb = max / 1024 / 1024;
        return Err(AppError::validation(format!(
            "Download failed: total file size {cur_mb}MB exceeds limit {max_mb}MB"
        )));
    }

    zip::pack_download(src, tmp.clone(), opts).await?;
    let bytes = tokio::fs::read(&tmp).await?;
    let _ = tokio::fs::remove_file(&tmp).await;
    Ok(zip_response(&filename, bytes))
}

// ── files-update / all-files-update (复用 code_service path 制核心, 无 zip 版本备份) ──

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FilesUpdateBody {
    user_id: String,
    c_id: String,
    files: Vec<code_service::FileOp>,
    #[serde(default)]
    custom_target_dir: Option<String>,
}

/// `POST /api/computer/files-update` (对齐 nuwax computer updateFiles; 增量 create/delete/rename/modify)。
async fn files_update(
    State(state): State<AppState>,
    Json(mut body): Json<FilesUpdateBody>,
) -> Result<Json<Value>, AppError> {
    let path = resolve_computer_target(
        &state,
        &body.user_id,
        &body.c_id,
        body.custom_target_dir.as_deref(),
    );
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

// ── upload-file / upload-files (multipart, path 制, 无版本备份) ───────────────────

/// `POST /api/computer/upload-file` (对齐 nuwax computer uploadFile; multipart)。
/// 返回 {success, message, fileSize} (不返回 filePath/originalname)。
async fn upload_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut file_path = None;
    let mut custom_target_dir = None;
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
            "customTargetDir" => custom_target_dir = Some(text_field(field).await?),
            "file" => data = Some(bytes_field(field).await?),
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    let file_path = file_path.ok_or_else(|| AppError::validation("filePath is required"))?;
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;
    let ws = resolve_computer_target(&state, &user_id, &cid, custom_target_dir.as_deref());
    let target = path_safety::ensure_within(&ws, &file_path)?;
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let file_size = data.len();
    tokio::fs::write(&target, data).await?;
    Ok(Json(json!({
        "success": true,
        "message": "File uploaded successfully",
        "fileSize": file_size,
    })))
}

/// `POST /api/computer/upload-files` (对齐 nuwax computer uploadFiles; 多文件 multipart)。
/// 返回 {success, message, totalCount, successCount, failCount, results:[{success,filePath,originalname?,message?,fileSize?,error?}]}。
async fn upload_files(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut custom_target_dir = None;
    let mut file_paths: Vec<String> = Vec::new();
    let mut files_vec: Vec<(Option<String>, Vec<u8>)> = Vec::new();
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
                files_vec.push((original, bytes_field(field).await?));
            }
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    if file_paths.len() != files_vec.len() {
        return Err(AppError::validation("filePaths and files count mismatch"));
    }
    let ws = resolve_computer_target(&state, &user_id, &cid, custom_target_dir.as_deref());
    let total = file_paths.len();
    let mut success_count = 0usize;
    let mut results: Vec<Value> = Vec::new();
    for (fp, (original, data)) in file_paths.iter().zip(files_vec) {
        // 空文件对象
        if data.is_empty() {
            results.push(json!({
                "success": false,
                "filePath": fp,
                "error": "Empty file object",
            }));
            continue;
        }
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
        let file_size = data.len();
        match write_file_create_parent(&target, data).await {
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
async fn write_file_create_parent(target: &Path, data: Vec<u8>) -> Result<(), std::io::Error> {
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(target, data).await
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
/// 返回 {success, cleaned} (字段 cleaned, 非 removed; 无 message)。
/// 递归找 scripts/package-platforms.mjs 所在 projectDir, 删其 dist-packages (对齐 nuwax)。
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
    let ws = resolve_computer_target(
        &state,
        user_id,
        cid,
        body.get("customTargetDir").and_then(|v| v.as_str()),
    );
    if !ws.exists() {
        return Ok(Json(json!({ "success": true, "cleaned": false })));
    }
    let skip = package_search_skip_dirs(&state.config.zip_workspace_exclude);
    let project_dir = match find_package_script(&ws, &skip).await {
        Some(d) => d,
        None => return Ok(Json(json!({ "success": true, "cleaned": false }))),
    };
    let dist = project_dir.join("dist-packages");
    let cleaned = if dist.exists() {
        match tokio::fs::remove_dir_all(&dist).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "cleanup dist-packages failed");
                false
            }
        }
    } else {
        false
    };
    Ok(Json(json!({ "success": true, "cleaned": cleaned })))
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
    let res =
        crate::service::computer_ws::create_workspace(&ws, skill_zip, Vec::new(), None).await?;
    Ok(Json(json!({
        "success": true,
        "message": res.message,
        "workspaceRoot": res.workspace_root,
        "updatedSkills": res.updated_skills,
    })))
}

// ── create-workspace-v2 (multipart + skillUrls + mcp/hooks/permissions/hookScripts) ──

/// `POST /api/computer/create-workspace-v2` (对齐 nuwax create-workspace-v2):
/// multipart: userId, cId, file, skillUrls, mcpServersConfig, hooksConfig,
/// permissionsConfig, hookScripts。skillUrls/hookScripts 若为 JSON 字符串则解析。
/// 复用 computer_ws::create_workspace + write_agent_hook_configs。
async fn create_workspace_v2(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut skill_zip: Option<Vec<u8>> = None;
    let mut file_name = None;
    let mut skill_urls: Vec<String> = Vec::new();
    let mut mcp_servers_config: Option<String> = None;
    let mut hooks_config: Option<String> = None;
    let mut permissions_config: Option<String> = None;
    let mut hook_scripts_raw: Option<String> = None;
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
            "skillUrls" => {
                let t = text_field(field).await?;
                if let Ok(urls) = serde_json::from_str::<Vec<String>>(&t) {
                    skill_urls.extend(urls);
                } else {
                    skill_urls.push(t);
                }
            }
            "mcpServersConfig" => mcp_servers_config = Some(text_field(field).await?),
            "hooksConfig" => hooks_config = Some(text_field(field).await?),
            "permissionsConfig" => permissions_config = Some(text_field(field).await?),
            "hookScripts" => hook_scripts_raw = Some(text_field(field).await?),
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    if skill_zip.is_some() {
        validate_zip_ext(file_name.as_deref())?;
    }
    // hookScripts: JSON 字符串 → Vec<HookScript>, 解析失败 → None (对齐 nuwax)
    let hook_scripts = hook_scripts_raw.and_then(|s| {
        serde_json::from_str::<Vec<crate::service::agent_hooks::HookScript>>(&s).ok()
    });
    let hook_input = crate::service::agent_hooks::HookConfigInput {
        mcp_servers_config,
        hooks_config,
        permissions_config,
        hook_scripts,
    };
    let ws = ws_path(&state, &user_id, &cid);
    tokio::fs::create_dir_all(&ws).await?;
    let res =
        crate::service::computer_ws::create_workspace(&ws, skill_zip, skill_urls, Some(hook_input))
            .await?;
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
    // message 对齐 nuwax pushSkillsToWorkspace: 有 skills → "Pushed N skills: a, b";
    // 无 → "No valid skill directories found in file or skillUrls"
    let message = if updated.is_empty() {
        "No valid skill directories found in file or skillUrls".to_string()
    } else {
        format!("Pushed {} skills: {}", updated.len(), updated.join(", "))
    };
    Ok(Json(json!({
        "success": true,
        "message": message,
        "workspaceRoot": ws.display().to_string(),
        "updatedSkills": updated,
    })))
}

// ── init-project-template (解压模板 zip + 可选 git init) ──────────────────────────

/// `POST /api/computer/init-project-template` (对齐 nuwax initProjectTemplate)。
/// multipart: userId, cId, file(模板 zip), enableGit。解压到工作区。
/// git 触发双开关: GIT_ENABLED && enableGit → init + commit (对齐 nuwax)。
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
    // git 双开关: GIT_ENABLED && enableGit → init + initial commit (对齐 nuwax)
    if state.config.git_enabled && enable_git {
        let an = state.config.git_default_author_name.clone();
        let ae = state.config.git_default_author_email.clone();
        // init_repo 内部已含 initial commit (ensure_repo + ensure_gitignore + commit_indexed)
        let _ = crate::service::git::init_repo(&ws, &an, &ae);
    }
    Ok(Json(json!({
        "success": true,
        "message": "Project template initialized successfully",
        "workspaceRoot": ws.display().to_string(),
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
/// → 解析 stdout 中产物 (path 转 workspace 相对, platform 从文件名提取)。响应无 stdout。
async fn build_agent_package(
    State(state): State<AppState>,
    Json(body): Json<BuildAgentBody>,
) -> Result<Json<Value>, AppError> {
    let ws = ws_path(&state, &body.user_id, &body.c_id);
    if !ws.exists() {
        return Err(AppError::resource("workspace does not exist"));
    }
    // 递归找 scripts/package-platforms.mjs 所在目录 (对齐 nuwax findPackageScript;
    // skip ZIP_WORKSPACE_EXCLUDE ∪ {dist-packages}, 而非仅 package.json)
    let skip = package_search_skip_dirs(&state.config.zip_workspace_exclude);
    let pkg_dir = find_package_script(&ws, &skip)
        .await
        .ok_or_else(|| AppError::business("package-platforms.mjs not found in workspace"))?;
    let timeout = state.config.dev_command_timeout_secs;
    // pnpm install 前准备 .npmrc (best-effort, 对齐 nuwax runPnpmInstall → ensurePnpmInstallConfig)
    pnpm_config::ensure_pnpm_install_config(&pkg_dir).await;
    // pnpm install (含 devDependencies; esbuild/typescript 在 devDependencies 中)
    let (_, _, code) = run_capture("pnpm", &["install"], &pkg_dir, timeout).await?;
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
    // 解析产物 (path 转 workspace 相对, platform 从文件名提取; 无 stdout 字段)
    let artifacts = parse_artifacts(&stdout, &ws);
    Ok(Json(json!({ "success": true, "artifacts": artifacts })))
}

/// 从 package-platforms stdout 解析产物列表: {path (workspace 相对), fileName, platform}。
/// path 对齐 nuwax: 相对 workspace 目录, 路径分隔符转 `/`。
fn parse_artifacts(stdout: &str, workspace: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let t = line.trim();
        if !(t.ends_with(".tar.gz")
            || t.ends_with(".tar.bz2")
            || t.ends_with(".zip")
            || t.ends_with(".tgz"))
        {
            continue;
        }
        // t 可能是相对路径或绝对路径; 统一转 workspace 相对
        let abs = if Path::new(t).is_absolute() {
            PathBuf::from(t)
        } else {
            workspace.join(t)
        };
        let rel = abs
            .strip_prefix(workspace)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| t.to_string());
        let file_name = t.rsplit('/').next().unwrap_or(t).to_string();
        let platform = extract_platform_from_filename(&file_name).unwrap_or_default();
        out.push(json!({
            "path": rel,
            "fileName": file_name,
            "platform": platform,
        }));
    }
    out
}

/// 从产物文件名提取 platform (对齐 nuwax extractPlatformFromFileName):
/// 去掉 .tar.gz/.tar.bz2/.tgz/.zip 后缀, 按 `-` 分割, 找最后一个形如 x.y.z 的版本段,
/// platform = parts[2..versionIdx] (跳过 agent-{id})。
fn extract_platform_from_filename(file_name: &str) -> Option<String> {
    let stem = file_name
        .strip_suffix(".tar.gz")
        .or_else(|| file_name.strip_suffix(".tar.bz2"))
        .or_else(|| file_name.strip_suffix(".tgz"))
        .or_else(|| file_name.strip_suffix(".zip"))
        .unwrap_or(file_name);
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 4 {
        return None;
    }
    // 找最后一个版本段 (形如 x.y.z)
    let mut version_idx = None;
    for (i, p) in parts.iter().enumerate().rev() {
        let is_version = p
            .split('.')
            .all(|seg| seg.bytes().all(|b| b.is_ascii_digit()))
            && p.contains('.')
            && !p.is_empty();
        if is_version {
            version_idx = Some(i);
            break;
        }
    }
    let vidx = version_idx?;
    if vidx <= 2 {
        return None;
    }
    // platform = parts[2..vidx]
    let platform = parts[2..vidx].join("-");
    if platform.is_empty() {
        None
    } else {
        Some(platform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_platform_handles_multi_segment() {
        // agent-{id}-{platform}-{ver}.{ext} → platform 可能多段
        assert_eq!(
            extract_platform_from_filename("agent-foo-linux-x64-1.0.0.zip"),
            Some("linux-x64".to_string())
        );
        assert_eq!(
            extract_platform_from_filename("agent-foo-darwin-2.1.0.tar.gz"),
            Some("darwin".to_string())
        );
        assert_eq!(
            extract_platform_from_filename("agent-bar-win32-x64-0.9.1.tgz"),
            Some("win32-x64".to_string())
        );
    }

    #[test]
    fn extract_platform_returns_none_when_no_version() {
        assert_eq!(
            extract_platform_from_filename("agent-foo-linux-x64.zip"),
            None
        );
        assert_eq!(extract_platform_from_filename("nope.zip"), None);
    }

    #[test]
    fn utf8_percent_encode_only_safe_chars_pass() {
        // [A-Za-z0-9-._~] 保留, 其余百分号编码
        assert_eq!(utf8_percent_encode("a-b.c_1~2.zip"), "a-b.c_1~2.zip");
        assert_eq!(utf8_percent_encode("a b.zip"), "a%20b.zip");
        // 中文: 中=E4%B8%AD, 文=E6%96%87
        assert_eq!(utf8_percent_encode("中文.zip"), "%E4%B8%AD%E6%96%87.zip");
    }
}
