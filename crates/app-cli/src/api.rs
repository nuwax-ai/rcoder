//! 管理 API（/health、/reload、/logs、/logs/<dir>/stream）。

use std::path::PathBuf;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
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
    let logs = crate::log_reader::list_log_files(&state.log_dir);
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

async fn get_logs(
    State(state): State<AppState>,
    Path(dir): Path<String>,
    Query(q): Query<LogQuery>,
) -> Json<Value> {
    let suffix = if q.err.unwrap_or(false) { ".err.log" } else { ".out.log" };
    let path = state.log_dir.join(format!("{dir}{suffix}"));
    match crate::log_reader::read_last_n_lines(&path, q.lines) {
        Ok((lines, total_bytes)) => Json(json!({
            "dir": dir,
            "totalBytes": total_bytes,
            "lines": lines,
            "truncated": total_bytes > 0 && (lines.len() as u64) < total_bytes / 50, // 粗略判断
        })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

// ── /logs/<dir>/stream（SSE 实时流 + Last-Event-ID 补漏）────────────────────

async fn stream_logs(
    State(state): State<AppState>,
    Path(dir): Path<String>,
    Query(q): Query<LogQuery>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let suffix = if q.err.unwrap_or(false) { ".err.log" } else { ".out.log" };
    let path = state.log_dir.join(format!("{dir}{suffix}"));

    // 读 Last-Event-ID（浏览器断线重连自动发）
    let start_offset = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| {
            // 没有 Last-Event-ID = 首次连接 → 从文件尾开始（只推新行）
            crate::log_reader::file_size(&path)
        });

    tracing::info!("SSE stream {dir} from offset {start_offset}");

    let stream = async_stream::stream! {
        let mut offset = start_offset;
        loop {
            let current_size = crate::log_reader::file_size(&path);
            if offset < current_size {
                if let Ok((lines, total)) = crate::log_reader::read_from_offset(&path, offset) {
                    for (line, byte_pos) in lines {
                        yield Ok(Event::default()
                            .id(byte_pos.to_string())
                            .data(line));
                    }
                    offset = total;
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };

    Sse::new(stream)
}
