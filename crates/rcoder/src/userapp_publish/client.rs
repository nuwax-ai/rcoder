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

use anyhow::{Context, anyhow};
use futures_util::StreamExt;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use shared_types::BuildProgressEvent;
use shared_types::HttpResult;
use tokio::sync::mpsc;

const REQUEST_TIMEOUT_SECS: u64 = 120;
const SSE_CHANNEL_CAP: usize = 128;
/// SSE 单帧缓冲上限:超过仍找不到帧分隔符视为异常(防无分隔符的恶意/异常流撑爆内存)。
const MAX_SSE_BUFFER: usize = 1024 * 1024;

/// build 触发响应的 data(`{taskId}`)。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildStarted {
    task_id: String,
}

/// build 任务快照的 data(`{status, releaseId, sha256, sizeBytes, fileName, error}`)。
/// 消费方:orchestrator(产物摘要三字段)、agent_runner(断流恢复终态判定)。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuildSnapshot {
    pub status: String,
    pub release_id: Option<String>,
    pub error: Option<String>,
    pub sha256: Option<String>,
    pub size_bytes: Option<u64>,
    pub file_name: Option<String>,
}

/// agent-runner(file-server)HttpResult 响应解析错误(结构化:code/message 字段保留,
/// 不揉进 anyhow 字符串;外层 anyhow Result 经 `?` 自动包装 Display)。
#[derive(Debug, thiserror::Error)]
enum FsReplyError {
    /// 受控失败:HTTP 非 2xx 或业务 code != SUCCESS。
    #[error("agent-runner {context} -> HTTP {http}: {message} (code={code})")]
    Failure {
        context: String,
        http: u16,
        code: String,
        message: String,
    },
    /// 业务成功但缺 data 字段(成功响应必有 data)。
    #[error("agent-runner {context}: success response missing data: {body}")]
    MissingData { context: String, body: String },
    /// 响应体不是 HttpResult 形态(网关 HTML / 全局 AppError 兜底等)。
    #[error("{detail}")]
    Shape { detail: anyhow::Error },
}

/// 解析 HttpResult 包装响应(直接复用 `shared_types::HttpResult`——file-server 的
/// UserAppReply 层输出同一类型,客户端消费同一契约,单一事实源,不另建镜像)。
///
/// 成功判定按权威语义 `code == SUCCESS`(HttpResult 的 success 字段是序列化期
/// 推导值、`#[serde(skip)]`,反序列化侧不可用)。
fn parse_http_result<T: DeserializeOwned>(
    body: &Value,
    status: reqwest::StatusCode,
    context: &str,
) -> Result<T, FsReplyError> {
    let reply: HttpResult<T> =
        serde_json::from_value(body.clone()).map_err(|_| FsReplyError::Shape {
            detail: extract_fs_error(body, status, context),
        })?;
    if !status.is_success() || reply.code != shared_types::error_codes::SUCCESS {
        return Err(FsReplyError::Failure {
            context: context.to_string(),
            http: status.as_u16(),
            code: reply.code,
            message: reply.message,
        });
    }
    reply.data.ok_or_else(|| FsReplyError::MissingData {
        context: context.to_string(),
        body: body.to_string(),
    })
}

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

/// 触发 agent-runner workspace build,返 taskId(类型化:HttpResult data.taskId)。
pub async fn trigger_build(addr: &str, app_id: &str) -> anyhow::Result<String> {
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
    let started: BuildStarted = parse_http_result(&body, status, "build")?;
    Ok(started.task_id)
}

/// 取消 agent-runner build 任务(软取消 + kill 进程组)。
pub async fn cancel_build(addr: &str, task_id: &str) -> anyhow::Result<()> {
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

/// 取 build 任务快照(类型化:HttpResult data → [`BuildSnapshot`])。
pub async fn get_build_snapshot(addr: &str, task_id: &str) -> anyhow::Result<BuildSnapshot> {
    let url = format!("{addr}/api/userapp/tasks/{task_id}");
    let resp = HTTP_CLIENT
        .get(&url)
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .send()
        .await
        .with_context(|| format!("agent-runner get task: {url}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.context("parse task snapshot")?;
    Ok(parse_http_result(&body, status, "get task")?)
}

/// 订阅 agent-runner build 进度 SSE → `mpsc::Receiver<Result<BuildProgressEvent, BuildStreamError>>`。
///
/// 后台 spawn:connect/非2xx/读取/缓冲错误经 channel 透传(orchestrator 拿到真因);接收端
/// 退出时 `tx.closed()` 触发及时收尾(不留挂连接);终态事件后关闭。
/// build 进度流的接收端（wait_build 断流重订阅时整体替换）。
pub type BuildProgressReceiver = mpsc::Receiver<Result<SseItem, BuildStreamError>>;

/// 一帧已解析的 build 进度事件：file-server 的 SSE `id:`（事件 seq，续传游标）+
/// 类型化事件。seq 由 file-server 侧 ring 分配（`Event::default().id(seq)`）。
#[derive(Debug, Clone)]
pub struct SseItem {
    pub seq: Option<u64>,
    pub event: BuildProgressEvent,
}

/// 订阅 build 进度 SSE（回放+实时）。`from_seq`：只回放 `seq >= from_seq` 的历史
/// （file-server 端点参数）——断流重订阅传 `最后收到的 seq + 1` 续传，避免缺省 0
/// 的全量回放把历史事件重复推给任务（前端日志整段重复、进度回跳）。
pub fn subscribe_build_progress(addr: &str, task_id: &str, from_seq: u64) -> BuildProgressReceiver {
    let (tx, rx) = mpsc::channel(SSE_CHANNEL_CAP);
    let url = format!("{addr}/api/userapp/tasks/{task_id}/logs/stream?from_seq={from_seq}");
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
                            match parse_sse_frame(&frame[..end]) {
                                Ok(item) => {
                                    let terminal = matches!(
                                        item.event,
                                        BuildProgressEvent::Completed { .. }
                                            | BuildProgressEvent::Failed { .. }
                                            | BuildProgressEvent::Cancelled
                                    );
                                    if tx.send(Ok(item)).await.is_err() {
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

/// 整体包下载 URL(rcoder prepare_release 的 url 字段;app_manager 据此从 agent-runner 拉包)。
pub fn package_url(addr: &str, app_id: &str, file_name: &str) -> String {
    format!("{addr}/api/userapp/static/{app_id}/{file_name}")
}

/// 从 SSE 帧提取 `id:`（事件 seq，可选）与 `data:` 行，反序列化为类型化
/// `BuildProgressEvent`（Fail Fast:错误带类型）。
fn parse_sse_frame(frame: &[u8]) -> Result<SseItem, SseParseError> {
    let text = std::str::from_utf8(frame)?;
    let seq = text
        .lines()
        .find_map(|l| l.strip_prefix("id:"))
        .and_then(|v| v.trim().parse::<u64>().ok());
    let data_line = text
        .lines()
        .find_map(|l| l.strip_prefix("data:").map(|s| s.trim_start()))
        .ok_or(SseParseError::NoDataLine)?;
    let event = serde_json::from_str(data_line)?;
    Ok(SseItem { seq, event })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_parsed_from_httpresult_data() {
        let snap: BuildSnapshot = parse_http_result(
            &json!({
                "code": "0000", "message": "Operation successful",
                "data": {"status": "completed", "releaseId": "rel-1", "sha256": "abc"},
                "success": true
            }),
            reqwest::StatusCode::OK,
            "get task",
        )
        .expect("valid snapshot");
        assert_eq!(snap.status, "completed");
        assert_eq!(snap.release_id.as_deref(), Some("rel-1"));
        assert_eq!(snap.sha256.as_deref(), Some("abc"));
        assert!(snap.size_bytes.is_none()); // 缺省字段 → None 而非反序列化失败
    }

    #[test]
    fn build_task_id_extracted_from_httpresult_data() {
        let started: BuildStarted = parse_http_result(
            &json!({
                "code": "0000", "message": "Operation successful",
                "data": {"taskId": "01a01a28", "status": "pending"},
                "success": true
            }),
            reqwest::StatusCode::OK,
            "build",
        )
        .expect("valid build response");
        assert_eq!(started.task_id, "01a01a28");
    }

    #[test]
    fn httpresult_rejects_missing_data_and_error_shape() {
        // 成功(code=SUCCESS)但缺 data → MissingData(成功响应必有 data)
        for invalid in [
            json!({"code": "0000", "message": "ok"}),
            json!({"code": "0000", "message": "ok", "data": null}),
        ] {
            let error = parse_http_result::<BuildStarted>(&invalid, reqwest::StatusCode::OK, "x")
                .expect_err("data is required");
            assert!(matches!(error, FsReplyError::MissingData { .. }), "{error}");
        }
        // 业务失败(code != SUCCESS)→ Failure,message/code 结构化透传
        let error = parse_http_result::<BuildStarted>(
            &json!({"code": "ERR_BUILD_FAILED", "message": "boom"}),
            reqwest::StatusCode::OK,
            "build",
        )
        .expect_err("failure reply");
        assert!(matches!(error, FsReplyError::Failure { .. }), "{error}");
        assert!(error.to_string().contains("boom"), "{error}");
        assert!(error.to_string().contains("ERR_BUILD_FAILED"), "{error}");
        // 旧顶层形态(无 code/message,taskId 在顶层)→ Shape 兜底拒绝
        let error = parse_http_result::<BuildStarted>(
            &json!({"success": true, "taskId": "01a01a28"}),
            reqwest::StatusCode::OK,
            "build",
        )
        .expect_err("top-level shape rejected");
        assert!(matches!(error, FsReplyError::Shape { .. }), "{error}");
    }

    #[test]
    fn parse_sse_frame_extracts_typed_event_and_seq() {
        let frame = b"id: 42\ndata: {\"event\":\"stage\",\"stage\":\"Build\"}\n\n";
        let item = parse_sse_frame(frame).expect("valid frame");
        assert_eq!(item.seq, Some(42));
        assert!(matches!(item.event, BuildProgressEvent::Stage { stage } if stage == "Build"));
        // 无 id: 行 → seq None（续传游标缺失，调用方保持旧值）
        let bare = parse_sse_frame(b"data: {\"event\":\"stage\",\"stage\":\"Build\"}\n\n")
            .expect("valid frame without id");
        assert_eq!(bare.seq, None);
        // 非数字 id 容错为 None（不判帧无效）
        let bad_id =
            parse_sse_frame(b"id: abc\ndata: {\"event\":\"stage\",\"stage\":\"Build\"}\n\n")
                .expect("bad id tolerated");
        assert_eq!(bad_id.seq, None);
    }

    #[test]
    fn parse_sse_frame_no_data_line_is_not_data() {
        // comment/keepalive 帧(无 data: 行)
        assert!(matches!(
            parse_sse_frame(b": keepalive\n\n"),
            Err(SseParseError::NoDataLine)
        ));
    }

    #[test]
    fn parse_sse_frame_bad_json_is_invalid() {
        assert!(matches!(
            parse_sse_frame(b"id: 7\ndata: {not json\n\n"),
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
