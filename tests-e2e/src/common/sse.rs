//! SSE 客户端：GET `/computer/progress/{session_id}`，手写帧解析。
//!
//! 行为对齐 Python 套件 `sse_collect`（tests/sse_e2e/common.py）：
//! - `event:` / `id:` / `data:` 三类行；`: keep-alive` 注释行与空行忽略
//! - `data:` 行到达且已有 event 才构成事件（axum SSE 帧序：event → id → data）
//! - 强制 UTF-8 解码（SSE data 是 UTF-8 JSON，防中文 mojibake）
//! - `idle_stop`：收到 `end_turn` 后再读 1s 无新行即返回（加速场景；
//!   服务端"终端即清"通常在 1s 内关流，连接自然结束）
//! - `duration` 总窗口由 deadline 控制（tokio sleep_until 竞争，非 reqwest 总超时）
//! - 每收一条事件经 `on_event` 回调实时交给调用方（JSONL 实时落盘）

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::Value;

/// 一条已解析的 SSE 事件（data 行反序列化为 JSON；解析失败时 data 为字符串值）。
#[derive(Debug, Clone)]
pub struct SseEvent {
    /// `id:` 行（seq；服务端合成消息可能无 id）
    pub seq: Option<u64>,
    /// `event:` 行（sub_type，如 agent_message_chunk / end_turn）
    pub event: String,
    /// `data:` 行解析后的 JSON
    pub data: Value,
    /// 相对 collect 开始的毫秒
    pub t_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndedReason {
    /// end_turn 后 1s 无新行（idle_stop）
    IdleAfterEndTurn,
    /// 总窗口到时
    Deadline,
    /// 服务端关闭连接（正常 EOF；如终端即清）
    StreamEnded,
    /// 连接/协议错误（非 200、读失败）
    Error(String),
}

impl EndedReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            EndedReason::IdleAfterEndTurn => "idle_after_end_turn",
            EndedReason::Deadline => "deadline",
            EndedReason::StreamEnded => "stream_ended",
            EndedReason::Error(_) => "error",
        }
    }
}

/// 收集 SSE 事件直至结束。返回 (事件列表, 结束原因)。
///
/// `on_event` 在每条事件解析出时同步回调（实时落盘）；回调开销须极小。
pub async fn collect<F>(
    http: &reqwest::Client,
    base_url: &str,
    session_id: &str,
    duration_s: f64,
    last_event_id: Option<u64>,
    idle_stop: bool,
    on_event: F,
) -> (Vec<SseEvent>, EndedReason)
where
    F: FnMut(&SseEvent),
{
    let url = format!("{base_url}/computer/progress/{session_id}");
    collect_at(http, &url, duration_s, last_event_id, idle_stop, on_event).await
}

/// 指定完整 URL 的收集变体（web 域 `/agent/progress/{sid}` 等非 computer 端点）。
pub async fn collect_at<F>(
    http: &reqwest::Client,
    url: &str,
    duration_s: f64,
    last_event_id: Option<u64>,
    idle_stop: bool,
    mut on_event: F,
) -> (Vec<SseEvent>, EndedReason)
where
    F: FnMut(&SseEvent),
{
    let mut req = http
        .get(url)
        // 对齐 Python 套件(requests 仅 HTTP/1.1)；axum SSE 对 h2 客户端的
        // 流行为未在本套件验证过，固定 1.1 排除协议层变量。
        .version(reqwest::Version::HTTP_11)
        .header("Accept", "text/event-stream")
        .header("Cache-Control", "no-cache");
    if let Some(id) = last_event_id {
        req = req.header("Last-Event-ID", id.to_string());
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return (Vec::new(), EndedReason::Error(format!("connect: {e}")));
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let preview: String = body.chars().take(200).collect();
        return (
            Vec::new(),
            EndedReason::Error(format!("HTTP {status}: {preview}")),
        );
    }

    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(duration_s.max(1.0));
    let mut events = Vec::new();
    let mut stream = resp.bytes_stream();
    let mut parser = SseParser::default();
    // idle_stop：end_turn 后 1s 即退出。退出期限并入 select 竞争（而非
    // 循环尾检查）——否则服务端不关流时会阻塞在 stream.next() 挂满整窗。
    let mut idle_deadline: Option<tokio::time::Instant> = None;

    loop {
        let chunk = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => {
                return (events, EndedReason::Deadline);
            }
            // end_turn 后 1s 到点（未见过 end_turn 时该分支永久 pending）
            _ = async {
                match idle_deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                return (events, EndedReason::IdleAfterEndTurn);
            }
            c = stream.next() => match c {
                None => return (events, EndedReason::StreamEnded),
                Some(Err(e)) => {
                    return (events, EndedReason::Error(format!("read: {e}")));
                }
                Some(Ok(chunk)) => chunk,
            },
        };

        for ev in parser.push(&chunk, started.elapsed().as_millis()) {
            on_event(&ev);
            let is_end_turn = ev.event == "end_turn";
            events.push(ev);
            if is_end_turn && idle_stop {
                idle_deadline
                    .get_or_insert_with(|| tokio::time::Instant::now() + Duration::from_secs(1));
            }
        }
    }
}

// SSE fields may arrive in any order and span transport chunks. Only an empty
// line dispatches a frame; EOF discards an unfinished frame. `seq` records this
// frame's explicit numeric ID, so an ID-less heartbeat never becomes evidence
// of another conversation event (the reconnect cursor is handled by callers).
#[derive(Default)]
struct SseParser {
    line: Vec<u8>,
    after_cr: bool,
    saw_line: bool,
    frame: SseFrame,
}

#[derive(Default)]
struct SseFrame {
    event: Option<String>,
    seq: Option<u64>,
    data: Vec<String>,
}

impl SseParser {
    fn push(&mut self, bytes: &[u8], t_ms: u128) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for &byte in bytes {
            if std::mem::take(&mut self.after_cr) && byte == b'\n' {
                continue;
            }
            if byte == b'\r' || byte == b'\n' {
                self.after_cr = byte == b'\r';
                let line = std::mem::take(&mut self.line);
                let line = String::from_utf8_lossy(&line);
                if let Some(event) = self.line(&line, t_ms) {
                    events.push(event);
                }
            } else {
                self.line.push(byte);
            }
        }
        events
    }

    fn line(&mut self, line: &str, t_ms: u128) -> Option<SseEvent> {
        let line = if std::mem::replace(&mut self.saw_line, true) {
            line
        } else {
            line.strip_prefix('\u{feff}').unwrap_or(line)
        };
        if line.is_empty() {
            let frame = std::mem::take(&mut self.frame);
            if frame.data.is_empty() {
                return None;
            }
            let data = frame.data.join("\n");
            return Some(SseEvent {
                seq: frame.seq,
                event: frame
                    .event
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| "message".into()),
                data: serde_json::from_str(&data).unwrap_or(Value::String(data)),
                t_ms,
            });
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.frame.event = Some(value.to_owned()),
            "id" if !value.contains('\0') => self.frame.seq = value.parse().ok(),
            "data" => self.frame.data.push(value.to_owned()),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
#[path = "sse_parser_tests.rs"]
mod parser_tests;

// ---------- 事件集分析 helper（移植 Python common.py 同名函数） ----------

/// 提取带 id 的事件序号（seq）。
pub fn ids_of(events: &[SseEvent]) -> Vec<u64> {
    events.iter().filter_map(|e| e.seq).collect()
}

/// 连接元事件（非对话消息）：acp-ts 后端 SSE 建立时的会话信息通知，
/// "终端清空"类断言只针对对话消息，元事件不计入。
pub const META_EVENTS: &[&str] = &["session_info_update"];

/// 过滤掉元事件后的对话消息事件。
pub fn message_events(events: &[SseEvent]) -> Vec<&SseEvent> {
    events
        .iter()
        .filter(|e| !META_EVENTS.contains(&e.event.as_str()))
        .collect()
}

/// 拼接 agent_message_chunk 的文本（SSE data JSON 内 data.content.text 双层路径）。
pub fn chunks_text(events: &[SseEvent]) -> String {
    let mut out = String::new();
    for e in events.iter().filter(|e| e.event == "agent_message_chunk") {
        if let Some(text) = e.data["data"]["content"]["text"].as_str() {
            out.push_str(text);
        }
    }
    out
}

/// seq 严格递增且无重复。
pub fn monotonic_unique(ids: &[u64]) -> bool {
    ids.windows(2).all(|w| w[1] > w[0])
}

/// 事件类型分布（BTreeMap 序列化为按 key 排序的 JSON object，报告可复现）。
pub fn type_counts(events: &[SseEvent]) -> Value {
    let mut counts = std::collections::BTreeMap::new();
    for e in events {
        *counts.entry(e.event.clone()).or_insert(0usize) += 1;
    }
    serde_json::to_value(counts).unwrap_or_default()
}

/// 24 字连续逐字重放检测（与 Python 套件一致）：a 中长度 win 且不含换行的
/// 片段若逐字出现在 b，说明 SSE chunk 被重放（语义复述不会命中）。
/// 返回命中的首个片段。
pub fn longest_common_snippet(a: &str, b: &str, win: usize) -> Option<String> {
    let a_chars: Vec<char> = a.chars().collect();
    if a_chars.len() <= win {
        return None;
    }
    let b_str = b;
    for i in 0..=(a_chars.len() - win) {
        let frag: String = a_chars[i..i + win].iter().collect();
        if frag.contains('\n') {
            continue;
        }
        if b_str.contains(&frag) {
            return Some(frag);
        }
    }
    None
}
