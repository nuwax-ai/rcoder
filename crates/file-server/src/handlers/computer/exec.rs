//! computer 执行/日志类 handlers: execute-command / get-logs。
//!
//! 包管理 (install-project / build-agent-package / cleanup-build-artifacts) 见
//! [`super::packages`]; 产物搜索/解析在 [`crate::service::package_build`]。

use std::path::{Path, PathBuf};

use axum::extract::State;
use garde::Validate;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{AppError, AppResult};
use crate::extract::{AppJson as Json, AppQuery as Query};

use super::process_capture::capture_command;
use super::{resolve_computer_target, ws_path};

// ── execute-command ─────────────────────────────────────────────────────────────

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExecCommandBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    c_id: String,
    #[garde(custom(crate::validation_rules::not_blank))]
    command: String,
}

/// 执行 shell 命令
///
/// 对齐 nuwax executeCommand; shell 执行 + 超时 + 捕获输出。
/// command 是 agent 提供的 shell 命令串, 故经 shell -c (与 nuwax child_process.exec 一致)。
/// shell 优先用 `BASH_PATH` (未配置则 sh); stdout/stderr 截断到 50MB (对齐 nuwax maxBuffer)。
#[utoipa::path(post, path = "/execute-command", request_body = ExecCommandBody, responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn execute_command(
    State(state): State<AppState>,
    Json(body): Json<ExecCommandBody>,
) -> Result<Json<Value>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    let cwd = ws_path(&state, &body.user_id, &body.c_id).await?;
    execute_command_impl(&state, cwd, &body.command).await
}

/// execute-command 的 workspace 无关实现 (cwd=workspace; command 经 shell -c)。
pub async fn execute_command_impl(
    state: &AppState,
    cwd: PathBuf,
    command: &str,
) -> Result<Json<Value>, AppError> {
    if !cwd.exists() {
        return Err(AppError::resource("workspace does not exist"));
    }
    let timeout_secs = state.config.dev_command_timeout_secs;
    // shell: BASH_PATH 非空则用之, 否则 sh (对齐 nuwax execOptions.shell)
    let shell = {
        let b = state.config.bash_path.trim();
        if b.is_empty() { "sh" } else { b }
    };
    let mut cmd = tokio::process::Command::new(shell);
    cmd.arg("-c").arg(command);
    cmd.current_dir(&cwd);
    cmd.env("NODE_ENV", "development");
    cmd.env_remove("CI");
    cmd.env_remove("NPM_CONFIG_PRODUCTION");
    let result = capture_command(&mut cmd, "execute-command", timeout_secs).await?;
    Ok(Json(json!({
        // TS 外层响应始终 success=true，命令结果由 exitCode 表示。
        "success": true,
        "stdout": result.stdout,
        "stderr": result.stderr,
        "exitCode": result.exit_code,
    })))
}

// ── get-logs ────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[garde(allow_unvalidated)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GetLogsQuery {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    c_id: String,
    #[serde(default = "default_tail_lines")]
    tail_lines: usize,
}
fn default_tail_lines() -> usize {
    200
}

/// 读取最新日志
///
/// 对齐 nuwax getLatestLogs; 读 .logs/ 下 mtime 最新的文件末尾 N 行。
/// 空场景区分 message, logFileName=null; 成功 message="Get log successfully"; 过滤空行。
#[utoipa::path(
    get,
    path = "/get-logs",
    params(GetLogsQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "Computer"
)]
pub(crate) async fn get_logs(
    State(state): State<AppState>,
    Query(q): Query<GetLogsQuery>,
) -> Result<Json<Value>, AppError> {
    q.validate().map_err(crate::error::from_garde)?;
    let log_dir = resolve_computer_target(&state, &q.user_id, &q.c_id, None)
        .await?
        .join(".logs");
    get_logs_impl(&state, log_dir, q.tail_lines).await
}

/// get-logs 的 workspace 无关实现 (log_dir={ws}/.logs, 由壳层拼好)。
pub async fn get_logs_impl(
    state: &AppState,
    log_dir: PathBuf,
    tail_lines: usize,
) -> Result<Json<Value>, AppError> {
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
    let content = crate::service::fs_util::read_to_string_bounded(
        &latest,
        state.config.log_read_max_bytes,
        "computer log",
    )
    .await?;
    // 过滤空行 (对齐 nuwax filter(l => l.length > 0))
    let all: Vec<&str> = content.split('\n').filter(|l| !l.is_empty()).collect();
    let total = all.len();
    let start = total.saturating_sub(tail_lines);
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
