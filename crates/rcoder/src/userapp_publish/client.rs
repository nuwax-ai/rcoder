//! rcoder → agent-runner file-server(`:60000`)HTTP client。
//!
//! 复刻 `create_sse_proxy_stream`(`handler/agent_session_notification.rs:1081`)模式:
//! reqwest + `bytes_stream()` + `\n\n` 分帧;区别在此处**解析**事件(提取 data JSON)而非透传。
//!
//! agent-runner 接口(file-server task 10):`POST /api/userapp/build`、`GET /tasks/{id}`、
//! `GET /tasks/{id}/logs/stream`(SSE)、`POST /tasks/{id}/cancel`、`GET /static/{app_id}/{file}`。

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;

const REQUEST_TIMEOUT_SECS: u64 = 120;
const SSE_CHANNEL_CAP: usize = 128;

/// reqwest::Client 内部持有连接池；进程内统一复用，普通请求通过 RequestBuilder 设置总超时，
/// SSE 长连接不设置总超时。
static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

/// file-server 统一错误体:`{success:false, error:{message}}` → 提取 message。
fn extract_fs_error(body: &Value, status: reqwest::StatusCode, where_: &str) -> anyhow::Error {
    let msg = body
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .or_else(|| body.get("message").and_then(|m| m.as_str()))
        .unwrap_or("unknown file-server error");
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

/// 取消 agent-runner build 任务(软取消 + kill 进程组,见 file-server cancel handler)。
pub async fn cancel_build(addr: &str, task_id: &str) -> Result<()> {
    let url = format!("{addr}/api/userapp/tasks/{task_id}/cancel");
    let resp = HTTP_CLIENT
        .post(&url)
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .send()
        .await
        .with_context(|| format!("agent-runner cancel request: {url}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or_else(|_| Value::Null);
    if !status.is_success() {
        return Err(extract_fs_error(&body, status, "cancel"));
    }
    Ok(())
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

/// 订阅 agent-runner build 进度 SSE → `mpsc::Receiver<data JSON>`(透传给前端)。
///
/// 后台 spawn 消费;终态事件(completed/failed/cancelled)后或接收端退出时关闭 channel。
pub fn subscribe_build_progress(addr: &str, task_id: &str) -> mpsc::Receiver<Value> {
    let (tx, rx) = mpsc::channel(SSE_CHANNEL_CAP);
    let url = format!("{addr}/api/userapp/tasks/{task_id}/logs/stream");
    tokio::spawn(async move {
        // SSE 长连接不设置总超时(build 可能数分钟到 1800s)，但复用同一个连接池。
        let resp = match HTTP_CLIENT
            .get(&url)
            .header("Accept", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                tracing::error!(status = %r.status(), "build sse non-2xx: {url}");
                return;
            }
            Err(e) => {
                tracing::error!(error = %e, "build sse connect failed: {url}");
                return;
            }
        };
        let mut stream = resp.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, "build sse read chunk failed");
                    break;
                }
            };
            buffer.extend_from_slice(&chunk);
            // 按双换行分帧
            while let Some(end) = buffer.windows(2).position(|w| w == b"\n\n") {
                let frame = buffer[..end].to_vec();
                buffer = buffer[end + 2..].to_vec();
                if let Some(data) = parse_sse_data(&frame) {
                    let terminal = is_terminal_event(&data);
                    if tx.send(data).await.is_err() {
                        return; // 接收端(orchestrator)已退出
                    }
                    if terminal {
                        return;
                    }
                }
            }
        }
    });
    rx
}

fn extract_task_snapshot(body: &Value) -> Result<Value> {
    body.get("task")
        .filter(|task| task.is_object())
        .cloned()
        .ok_or_else(|| anyhow!("agent-runner get task response missing object field 'task'"))
}

/// 整体包下载 URL(rcoder prepare_release 的 url 字段;rcoder app_manager 据此从 agent-runner 拉包)。
pub fn package_url(addr: &str, app_id: &str, file_name: &str) -> String {
    format!("{addr}/api/userapp/static/{app_id}/{file_name}")
}

/// 从 SSE 帧提取 `data:` 行 JSON。
fn parse_sse_data(frame: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(frame).ok()?;
    let data_line = text
        .lines()
        .find_map(|l| l.strip_prefix("data:").map(|s| s.trim_start()))?;
    serde_json::from_str(data_line).ok()
}

/// data JSON 是否终态事件(file-server BuildProgressEvent: completed/failed/cancelled)。
fn is_terminal_event(data: &Value) -> bool {
    matches!(
        data.get("event").and_then(|e| e.as_str()),
        Some("completed") | Some("failed") | Some("cancelled")
    )
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
}
