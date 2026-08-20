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
    mut on_event: F,
) -> (Vec<SseEvent>, EndedReason)
where
    F: FnMut(&SseEvent),
{
    let url = format!("{base_url}/computer/progress/{session_id}");
    let mut req = http
        .get(&url)
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
    let mut buf: Vec<u8> = Vec::new();
    // 当前帧的解析状态（对齐 axum SSE 帧序 event→id→data）
    let mut cur_event: Option<String> = None;
    let mut cur_seq: Option<u64> = None;
    let mut end_turn_at: Option<Instant> = None;

    loop {
        let chunk = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => {
                return (events, EndedReason::Deadline);
            }
            c = stream.next() => match c {
                None => return (events, EndedReason::StreamEnded),
                Some(Err(e)) => {
                    return (events, EndedReason::Error(format!("read: {e}")));
                }
                Some(Ok(chunk)) => chunk,
            },
        };

        buf.extend_from_slice(&chunk);
        // 按 \n 切行（残留 tail 留在 buf 等下一个 chunk）
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim(); // 去 \n 与 \r

            if let Some(name) = line.strip_prefix("event:") {
                cur_event = Some(name.trim().to_owned());
            } else if let Some(id) = line.strip_prefix("id:") {
                cur_seq = id.trim().parse::<u64>().ok();
            } else if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();
                if let Some(event) = cur_event.clone() {
                    let parsed = serde_json::from_str::<Value>(data)
                        .unwrap_or_else(|_| Value::String(data.to_owned()));
                    let ev = SseEvent {
                        seq: cur_seq,
                        event: event.clone(),
                        data: parsed,
                        t_ms: started.elapsed().as_millis(),
                    };
                    on_event(&ev);
                    let is_end_turn = event == "end_turn";
                    events.push(ev);
                    cur_event = None;
                    cur_seq = None;
                    if is_end_turn && idle_stop {
                        end_turn_at.get_or_insert_with(Instant::now);
                    }
                }
            }
            // 注释行（: keep-alive）与空行：忽略
        }

        // end_turn 已见且超 1s 无新行 → 提前返回
        if let Some(seen) = end_turn_at
            && seen.elapsed() > Duration::from_secs(1)
        {
            return (events, EndedReason::IdleAfterEndTurn);
        }
    }
}

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
