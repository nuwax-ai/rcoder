//! rcoder → agent-runner file-server(`:60000`)HTTP client。
//!
//! reqwest + `bytes_stream()` + SSE 分帧(支持 `\n\n` 与 `\r\n\r\n`)。
//! SSE 后台任务用类型化错误通道 + `tx.closed()` select:connect/读取/缓冲错误透传给
//! orchestrator(不再 log-then-close 让它瞎),接收端退出时及时收尾(不留挂连接)。
//!
//! agent-runner 接口:`POST /api/userapp/build`、`GET /tasks/{id}`、
//! `GET /tasks/{id}/logs/stream`(SSE)、`POST /tasks/{id}/cancel`、`GET /static/{app_id}/{file}`。

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use serde_json::{Value, json};
use shared_types::BuildProgressEvent;
use tokio::sync::mpsc;

const REQUEST_TIMEOUT_SECS: u64 = 120;
const SSE_CHANNEL_CAP: usize = 128;
/// SSE 单帧缓冲上限:超过仍找不到帧分隔符视为异常(防无分隔符的恶意/异常流撑爆内存)。
const MAX_SSE_BUFFER: usize = 1024 * 1024;

/// SSE 后台任务产生的流级错误(经 channel 透传给 orchestrator,不再静默 log-then-close)。
#[derive(thiserror::Error, Debug)]
pub enum BuildStreamError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("non-2xx {status}: {url}")]
    Non2xx { status: u16, url: String },
    #[error("read: {0}")]
    Read(String),
    #[error("buffer overrun {size} > {max} (no frame delimiter)")]
    BufferOverrun { size: usize, max: usize },
}

/// SSE 帧解析错误(帧级,log+skip,不入 channel —— 流继续)。
#[derive(thiserror::Error, Debug)]
enum SseParseError {
    #[error("non-utf8 frame: {0}")]
    NonUtf8(#[from] std::str::Utf8Error),
    /// 无 `data:` 行(comment/keepalive/非 data 事件)—— 正常 SSE 噪声,静默跳过。
    #[error("no data: line")]
    NoDataLine,
    #[error("invalid json: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

/// reqwest::Client 内部持有连接池;进程内统一复用。普通请求经 RequestBuilder 设总超时;
/// SSE 长连接不设总超时(build 可能数分钟到 1800s),但给连接建立本身设 15s 超时。
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

/// file-server 统一错误体:`{success:false, error:{message}}` → 提取 message;非 JSON 体带预览。
fn extract_fs_error(body: &Value, status: reqwest::StatusCode, where_: &str) -> anyhow::Error {
    let msg = body
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .or_else(|| body.get("message").and_then(|m| m.as_str()))
        .unwrap_or("unknown file-server error");
    anyhow!("agent-runner {where_} -> HTTP {status}: {msg}")
}

/// 非 JSON 错误体:读有上限 bytes,试 JSON 提取 message;失败则带 body 预览(修 "unknown file-server error")。
fn fs_error_from_bytes(bytes: &[u8], status: reqwest::StatusCode, where_: &str) -> anyhow::Error {
    let preview_len = bytes.len().min(200);
    let preview = String::from_utf8_lossy(&bytes[..preview_len]);
    let msg = serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|b| {
            b.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_owned)
                .or_else(|| b.get("message").and_then(|m| m.as_str()).map(str::to_owned))
        })
        .unwrap_or_else(|| format!("non-JSON body ({} bytes): {}", bytes.len(), preview.trim()));
    anyhow!("agent-runner {where_} -> HTTP {status}: {msg}")
}

/// 触发 agent-runner workspace build,返 taskId。
pub async fn trigger_build(addr: &str, app_id: &str) -> Result<String> {
    let url = format!("{addr}/api/userapp/build");
    let resp = HTTP_CLIENT
        .post(&url)
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .json(&json!({ "appId": app_id }))
        .send()
        .await
        .with_context(|| format!("agent-runner build request: {url}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.context("parse build response")?;
    if !status.is_success() || body.get("success").and_then(|s| s.as_bool()) == Some(false) {
        return Err(extract_fs_error(&body, status, "build"));
    }
    body.get("taskId")
        .and_then(|t| t.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow!("build response missing taskId: {body}"))
}

/// 取消 agent-runner build 任务(软取消 + kill 进程组)。
pub async fn cancel_build(addr: &str, task_id: &str) -> Result<()> {
    let url = format!("{addr}/api/userapp/tasks/{task_id}/cancel");
    let resp = HTTP_CLIENT
        .post(&url)
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .send()
        .await
        .with_context(|| format!("agent-runner cancel request: {url}"))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    // 非 2xx:读有上限 body,JSON 优先,失败带预览(不再退化为 "unknown file-server error")。
    let bytes = resp.bytes().await.unwrap_or_default();
    // 限 4KB 防超大错误体
    let bytes = &bytes[..bytes.len().min(4 * 1024)];
    Err(fs_error_from_bytes(bytes, status, "cancel"))
}

/// 取 build 任务快照(`{task:{status,releaseId,...}}`)。
pub async fn get_build_snapshot(addr: &str, task_id: &str) -> Result<Value> {
    let url = format!("{addr}/api/userapp/tasks/{task_id}");
    let resp = HTTP_CLIENT
        .get(&url)
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .send()
        .await
        .with_context(|| format!("agent-runner get task: {url}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.context("parse task snapshot")?;
    if !status.is_success() || body.get("success").and_then(|s| s.as_bool()) == Some(false) {
        return Err(extract_fs_error(&body, status, "get task"));
    }
    extract_task_snapshot(&body)
}

/// 订阅 agent-runner build 进度 SSE → `mpsc::Receiver<Result<BuildProgressEvent, BuildStreamError>>`。
///
/// 后台 spawn:connect/非2xx/读取/缓冲错误经 channel 透传(orchestrator 拿到真因);接收端
/// 退出时 `tx.closed()` 触发及时收尾(不留挂连接);终态事件后关闭。
pub fn subscribe_build_progress(
    addr: &str,
    task_id: &str,
) -> mpsc::Receiver<Result<BuildProgressEvent, BuildStreamError>> {
    let (tx, rx) = mpsc::channel(SSE_CHANNEL_CAP);
    let url = format!("{addr}/api/userapp/tasks/{task_id}/logs/stream");
    tokio::spawn(async move {
        let resp = match HTTP_CLIENT
            .get(&url)
            .header("Accept", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                if let Err(e) = tx
                    .send(Err(BuildStreamError::Non2xx {
                        status: r.status().as_u16(),
                        url: url.clone(),
                    }))
                    .await
                {
                    tracing::warn!("build stream error event send failed (orchestrator gone): {e}");
                }
                return;
            }
            Err(e) => {
                if let Err(send_err) = tx.send(Err(BuildStreamError::Connect(e.to_string()))).await
                {
                    tracing::warn!(
                        "build stream error event send failed (orchestrator gone): {send_err}"
                    );
                }
                return;
            }
        };
        let mut stream = resp.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            tokio::select! {
                biased;
                _ = tx.closed() => return, // 接收端(orchestrator)已退出 → 及时收尾
                chunk = stream.next() => match chunk {
                    None => return, // 流正常结束(无终态 → orchestrator 走 snapshot 恢复)
                    Some(Err(e)) => {
                        if let Err(send_err) = tx
                            .send(Err(BuildStreamError::Read(e.to_string())))
                            .await
                        {
                            tracing::warn!(
                                "build stream error event send failed (orchestrator gone): {send_err}"
                            );
                        }
                        return;
                    }
                    Some(Ok(chunk)) => {
                        buffer.extend_from_slice(&chunk);
                        while let Some((end, delim_len)) = find_frame_end(&buffer) {
                            let frame: Vec<u8> = buffer.drain(..end + delim_len).collect();
                            match parse_sse_data(&frame[..end]) {
                                Ok(ev) => {
                                    let terminal = matches!(
                                        ev,
                                        BuildProgressEvent::Completed { .. }
                                            | BuildProgressEvent::Failed { .. }
                                            | BuildProgressEvent::Cancelled
                                    );
                                    if tx.send(Ok(ev)).await.is_err() {
                                        return;
                                    }
                                    if terminal {
                                        return;
                                    }
                                }
                                Err(SseParseError::NoDataLine) => {} // comment/keepalive/非 data → 跳过
                                Err(e) => {
                                    // 数据帧损坏:warn 带上下文(不再静默丢),帧级跳过(流继续)。
                                    tracing::warn!(
                                        error = %e,
                                        frame_len = end,
                                        "build sse data frame parse failed, skipping"
                                    );
                                }
                            }
                        }
                        if buffer.len() > MAX_SSE_BUFFER {
                            if let Err(e) = tx
                                .send(Err(BuildStreamError::BufferOverrun {
                                    size: buffer.len(),
                                    max: MAX_SSE_BUFFER,
                                }))
                                .await
                            {
                                tracing::warn!(
                                    "build stream error event send failed (orchestrator gone): {e}"
                                );
                            }
                            return;
                        }
                    }
                },
            }
        }
    });
    rx
}

/// 找下一个 SSE 帧分隔符位置:返回 `(frame_end_exclusive, delim_len)`,支持 `\n\n` 与 `\r\n\r\n`。
fn find_frame_end(buffer: &[u8]) -> Option<(usize, usize)> {
    let nn = buffer.windows(2).position(|w| w == b"\n\n").map(|p| (p, 2));
    let crlf = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| (p, 4));
    match (nn, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn extract_task_snapshot(body: &Value) -> Result<Value> {
    body.get("task")
        .filter(|task| task.is_object())
        .cloned()
        .ok_or_else(|| anyhow!("agent-runner get task response missing object field 'task'"))
}

/// 整体包下载 URL(rcoder prepare_release 的 url 字段;app_manager 据此从 agent-runner 拉包)。
pub fn package_url(addr: &str, app_id: &str, file_name: &str) -> String {
    format!("{addr}/api/userapp/static/{app_id}/{file_name}")
}

/// 从 SSE 帧提取 `data:` 行,反序列化为类型化 `BuildProgressEvent`(Fail Fast:错误带类型)。
fn parse_sse_data(frame: &[u8]) -> Result<BuildProgressEvent, SseParseError> {
    let text = std::str::from_utf8(frame)?;
    let data_line = text
        .lines()
        .find_map(|l| l.strip_prefix("data:").map(|s| s.trim_start()))
        .ok_or(SseParseError::NoDataLine)?;
    let ev = serde_json::from_str(data_line)?;
    Ok(ev)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_snapshot_requires_task_object() {
        let task = extract_task_snapshot(&json!({
            "success": true,
            "task": {"sha256": "abc"}
        }))
        .expect("valid task snapshot");
        assert_eq!(task["sha256"], "abc");

        for invalid in [
            json!({"success": true}),
            json!({"success": true, "task": null}),
            json!({"success": true, "task": "invalid"}),
        ] {
            let error = extract_task_snapshot(&invalid).expect_err("task object is required");
            assert!(error.to_string().contains("missing object field 'task'"));
        }
    }

    #[test]
    fn parse_sse_data_extracts_typed_event() {
        let frame = b"data: {\"event\":\"stage\",\"stage\":\"Build\"}\n\n";
        let ev = parse_sse_data(frame).expect("valid frame");
        assert!(matches!(ev, BuildProgressEvent::Stage { stage } if stage == "Build"));
    }

    #[test]
    fn parse_sse_data_no_data_line_is_not_data() {
        // comment/keepalive 帧(无 data: 行)
        assert!(matches!(
            parse_sse_data(b": keepalive\n\n"),
            Err(SseParseError::NoDataLine)
        ));
    }

    #[test]
    fn parse_sse_data_bad_json_is_invalid() {
        assert!(matches!(
            parse_sse_data(b"data: {not json\n\n"),
            Err(SseParseError::InvalidJson(_))
        ));
    }

    #[test]
    fn find_frame_end_supports_crlf_and_lf() {
        assert_eq!(find_frame_end(b"data: x\n\nmore"), Some((7, 2)));
        assert_eq!(find_frame_end(b"data: x\r\n\r\nmore"), Some((7, 4)));
        assert_eq!(find_frame_end(b"no delim here"), None);
    }

    #[test]
    fn fs_error_from_bytes_non_json_includes_preview() {
        let err = fs_error_from_bytes(b"plain text error", reqwest::StatusCode::BAD_GATEWAY, "x");
        let msg = err.to_string();
        assert!(msg.contains("non-JSON body"), "{msg}");
        assert!(msg.contains("plain text error"), "{msg}");
    }

    #[test]
    fn fs_error_from_bytes_json_extracts_message() {
        let err = fs_error_from_bytes(
            br#"{"error":{"message":"boom"}}"#,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "x",
        );
        assert!(err.to_string().contains("boom"), "{}", err);
    }
}
