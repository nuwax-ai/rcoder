//! 管理 API（/health、/reload、/logs、/logs/<dir>/stream）。

use std::path::PathBuf;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::{Json, Router};
use axum::routing::{get, post};
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::{json, Value};

/// 启动管理 API（阻塞，由 main.rs 在 tokio::spawn 里跑）。
pub async fn serve(addr: &str, log_dir: PathBuf) {
    let state = AppState { log_dir };
    let app = Router::new()
        .route("/health", get(health))
        .route("/reload", post(reload))
        .route("/logs", get(list_logs))
        .route("/logs/{dir}", get(get_logs))
        .route("/logs/{dir}/stream", get(stream_logs))
        .with_state(state);

    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            tracing::info!("📡 管理 API 监听 http://{addr}");
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("管理 API 异常: {e}");
            }
        }
        Err(e) => tracing::warn!("⚠️  管理 API 绑定 {addr} 失败: {e}"),
    }
}

#[derive(Clone)]
struct AppState {
    log_dir: PathBuf,
}

// ── /health ───────────────────────────────────────────────────────────────────────

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "service": "app-cli" }))
}

// ── /reload ───────────────────────────────────────────────────────────────────────

async fn reload() -> Json<Value> {
    // TODO: 读 workspace manifest → 重生成 pingap config → 写 pingap.toml
    //       → pingap --autoreload 检测文件变化热生效
    Json(json!({ "status": "todo", "message": "dynamic reload not yet implemented" }))
}

// ── /logs（列举）──────────────────────────────────────────────────────────────

async fn list_logs(State(state): State<AppState>) -> Json<Value> {
    let logs = crate::log::reader::list_log_files(&state.log_dir);
    Json(serde_json::to_value(&logs).unwrap_or(json!([])))
}

// ── /logs/<dir>（读历史 tail N 行）────────────────────────────────────────────

#[derive(Deserialize)]
struct LogQuery {
    #[serde(default = "default_lines")]
    lines: usize,
    #[serde(default)]
    err: Option<bool>,
}

fn default_lines() -> usize {
    1000
}

/// 子项目目录名安全校验：仅允许 `[A-Za-z0-9._-]`，禁止空 / `.` / `..` / 路径分隔符。
/// `dir` 来自 URL，无校验拼路径会 `../` 穿越 `log_dir` 读到容器内任意文件。
fn is_safe_dir(dir: &str) -> bool {
    !dir.is_empty()
        && dir != "."
        && dir != ".."
        && !dir.contains('/')
        && !dir.contains('\\')
        && dir
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// 解析日志文件路径（带穿越校验）：`<log_dir>/<dir>.<out|err>.log`。
fn resolve_log_path(
    log_dir: &std::path::Path,
    dir: &str,
    err: bool,
) -> Result<PathBuf, (StatusCode, String)> {
    if !is_safe_dir(dir) {
        return Err((StatusCode::BAD_REQUEST, format!("invalid log dir: {dir}")));
    }
    let suffix = if err { ".err.log" } else { ".out.log" };
    Ok(log_dir.join(format!("{dir}{suffix}")))
}

async fn get_logs(
    State(state): State<AppState>,
    Path(dir): Path<String>,
    Query(q): Query<LogQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let path = resolve_log_path(&state.log_dir, &dir, q.err.unwrap_or(false))?;
    Ok(match crate::log::reader::read_last_n_lines(&path, q.lines) {
        Ok((lines, total_bytes, has_more)) => Json(json!({
            "dir": dir,
            "totalBytes": total_bytes,
            "lines": lines,
            "truncated": has_more,
        })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    })
}

// ── /logs/<dir>/stream（SSE 实时流 + Last-Event-ID 补漏）────────────────────

async fn stream_logs(
    State(state): State<AppState>,
    Path(dir): Path<String>,
    Query(q): Query<LogQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, (StatusCode, String)> {
    let path = resolve_log_path(&state.log_dir, &dir, q.err.unwrap_or(false))?;

    // 读 Last-Event-ID（浏览器断线重连自动发）
    let start_offset = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| {
            // 没有 Last-Event-ID = 首次连接 → 从文件尾开始（只推新行）
            crate::log::reader::file_size(&path)
        });

    tracing::info!("SSE stream {dir} from offset {start_offset}");

    let stream = async_stream::stream! {
        let mut offset = start_offset;
        loop {
            let current_size = crate::log::reader::file_size(&path);
            // 日志轮转/截断检测：writer rotate 后新文件从 0 开始增长，
            // 此时 offset（旧文件的字节位置）> current_size。若不重置，
            // `offset < current_size` 恒为 false → SSE 永久断流、丢日志。
            if current_size < offset {
                tracing::info!(
                    "log rotated/truncated for {dir}: offset {offset} > size {current_size}, resetting to 0"
                );
                offset = 0;
            }
            if let Ok((lines, total)) = crate::log::reader::read_from_offset(&path, offset) {
                for (line, byte_pos) in lines {
                    yield Ok(Event::default()
                        .id(byte_pos.to_string())
                        .data(line));
                }
                offset = total;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };

    Ok(Sse::new(stream))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_dir_allows_normal_names() {
        assert!(is_safe_dir("frontend"));
        assert!(is_safe_dir("backend-go"));
        assert!(is_safe_dir("api_v2"));
        assert!(is_safe_dir("service.api"));
    }

    #[test]
    fn safe_dir_rejects_traversal() {
        // 空与裸点
        assert!(!is_safe_dir(""));
        assert!(!is_safe_dir("."));
        assert!(!is_safe_dir(".."));
        // 路径分隔符（含 URL 解码后的 /）
        assert!(!is_safe_dir("../"));
        assert!(!is_safe_dir("../etc"));
        assert!(!is_safe_dir("foo/../bar"));
        assert!(!is_safe_dir("a/b"));
        assert!(!is_safe_dir("a\\b"));
        // 非 ASCII 与特殊符号
        assert!(!is_safe_dir("项目"));
        assert!(!is_safe_dir("a;b"));
        assert!(!is_safe_dir("a b"));
    }

    #[test]
    fn resolve_log_path_blocks_traversal() {
        let tmp = std::env::temp_dir();
        assert!(resolve_log_path(&tmp, "..", false).is_err());
        assert!(resolve_log_path(&tmp, "../../etc/passwd", false).is_err());
        assert!(resolve_log_path(&tmp, "ok", true).is_ok());
    }
}
