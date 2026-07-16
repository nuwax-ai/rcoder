//! `/api/computer` 路由 (对齐 nuwax computerRoutes)。
//!
//! computer 工作区路径: `{COMPUTER_WORKSPACE_ROOT}/{userId}/{cId}/`。
//! 本文件含 6 个可直接实现的路由 (路径制/简单 spawn); files-update / upload /
//! create-workspace / build-agent-package 等依赖 resolver+ctx 改造的路由见后续。

use std::path::PathBuf;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};
use crate::service::{tree, zip};
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
}

fn ws_path(state: &AppState, user_id: &str, cid: &str) -> PathBuf {
    state.resolver.resolve_computer(&ComputerContext {
        user_id: user_id.to_string(),
        cid: cid.to_string(),
    })
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

/// 打包 computer 工作区为 zip 字节流返回。
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
    let excludes = state.config.zip_workspace_exclude.clone();
    zip::pack_dir(src.clone(), tmp.clone(), exclude, excludes).await?;
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

// 下批实现 (依赖 resolver+ctx 改造): files-update / upload-file / upload-files /
// import-project / create-workspace[-v2] / push-skills-to-workspace[-v2] /
// init-project-template / build-agent-package / cleanup-build-artifacts
