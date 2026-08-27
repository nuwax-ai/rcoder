//! computer 执行/日志类 handlers: execute-command / get-logs。
//!
//! 包管理 (install-project / build-agent-package / cleanup-build-artifacts) 见
//! [`super::packages`]; 产物搜索/解析在 [`crate::service::package_build`]。

use axum::extract::State;
use garde::Validate;
use serde_json::Value;

use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::models::{ExecCommandBody, GetLogsQuery};

use crate::ops::{execute_command_impl, get_logs_impl};

use super::{resolve_computer_target, ws_path};

// ── execute-command ─────────────────────────────────────────────────────────────

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

// ── get-logs ────────────────────────────────────────────────────────────────────

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
