//! 场景编排公共件：collect 全流程留痕 + 后台 chat（spawn + 结果回写）。
//! compose 与 K8s 场景共用（入口 URL 参数化）。

use std::time::Duration;

use crate::common::report::{ChatTrace, JsonlReporter};
use crate::common::sse::{self, EndedReason, SseEvent};
use crate::common::{Env, chat_via, sanitize_request};

/// collect 留痕参数（collect_reported 的结构化入参）。
pub struct CollectSpec<'a> {
    pub phase: &'a str,
    /// 本次订阅的入口 URL（K8s 多入口场景用于区分，compose 即 rcoder 地址）
    pub entry: &'a str,
    pub sid: &'a str,
    pub duration_s: f64,
    pub last_event_id: Option<u64>,
    pub idle_stop: bool,
}

/// collect 全流程留痕：subscribe_begin → 实时 sse_event → subscribe_end 汇总。
pub async fn collect_reported(
    env: &Env,
    report: &JsonlReporter,
    spec: CollectSpec<'_>,
) -> (Vec<SseEvent>, EndedReason) {
    report.subscribe_begin(spec.phase, spec.entry, spec.last_event_id);
    let (events, ended) = sse::collect(
        &env.sse_http,
        spec.entry,
        spec.sid,
        spec.duration_s,
        spec.last_event_id,
        spec.idle_stop,
        |ev| report.sse_event(spec.phase, ev.seq, &ev.event, &ev.data, ev.t_ms),
    )
    .await;
    let ids = sse::ids_of(&events);
    let text = sse::chunks_text(&events);
    // Error 结束原因带详情（HTTP 状态/连接错误），供 jsonl 排查
    let ended_reason = match &ended {
        EndedReason::Error(detail) => format!("error: {detail}"),
        other => other.as_str().to_owned(),
    };
    report.subscribe_end(
        spec.phase,
        &ended_reason,
        events.len(),
        &ids,
        sse::type_counts(&events),
        &text,
    );
    (events, ended)
}

/// 后台 chat（续话场景：同步等返回会错过 SSE 窗口）。返回 JoinHandle，
/// 调用方在 collect 结束后 await 并将结果经 `record_bg_chat` 落 jsonl。
pub fn spawn_chat(
    env: &Env,
    url: &str,
    req: shared_types::ComputerChatRequest,
) -> tokio::task::JoinHandle<anyhow::Result<shared_types::ChatResponse>> {
    let client = env.http.clone();
    let url = url.to_owned();
    tokio::spawn(async move { chat_via(&client, &url, &req).await })
}

/// 后台 chat 结果落 jsonl（在 collect 结束后调用，保证 chat_request 行时序完整）。
pub async fn record_bg_chat(
    report: &JsonlReporter,
    phase: &str,
    url: &str,
    req: &shared_types::ComputerChatRequest,
    handle: tokio::task::JoinHandle<anyhow::Result<shared_types::ChatResponse>>,
) {
    let started = std::time::Instant::now();
    let req_sanitized = sanitize_request(req);
    let trace = match tokio::time::timeout(Duration::from_secs(150), handle).await {
        Ok(Ok(Ok(data))) => ChatTrace {
            phase,
            url,
            ok: true,
            request_sanitized: req_sanitized,
            response: Some(&serde_json::to_value(&data).unwrap_or_default()),
            error: None,
            elapsed_ms: started.elapsed().as_millis(),
        },
        Ok(Ok(Err(e))) => ChatTrace {
            phase,
            url,
            ok: false,
            request_sanitized: req_sanitized,
            response: None,
            error: Some(&e.to_string()),
            elapsed_ms: started.elapsed().as_millis(),
        },
        Ok(Err(join_err)) => ChatTrace {
            phase,
            url,
            ok: false,
            request_sanitized: req_sanitized,
            response: None,
            error: Some(&format!("bg chat task: {join_err}")),
            elapsed_ms: started.elapsed().as_millis(),
        },
        Err(_) => ChatTrace {
            phase,
            url,
            ok: false,
            request_sanitized: req_sanitized,
            response: None,
            error: Some("bg chat timeout (150s)"),
            elapsed_ms: started.elapsed().as_millis(),
        },
    };
    report.chat_request(trace);
}

pub fn count_event(events: &[SseEvent], name: &str) -> usize {
    events.iter().filter(|e| e.event == name).count()
}
