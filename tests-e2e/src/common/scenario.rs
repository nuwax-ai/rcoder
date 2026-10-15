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

/// web 域 collect（`/agent/progress/{sid}` 端点版；computer 域用 collect_reported）。
pub async fn collect_reported_web(
    env: &Env,
    report: &JsonlReporter,
    phase: &str,
    entry: &str,
    sid: &str,
    duration_s: f64,
) -> (Vec<SseEvent>, EndedReason) {
    let url = format!("{entry}/agent/progress/{sid}");
    report.subscribe_begin(phase, &url, None);
    let (events, ended) = sse::collect_at(&env.sse_http, &url, duration_s, None, false, |ev| {
        report.sse_event(phase, ev.seq, &ev.event, &ev.data, ev.t_ms)
    })
    .await;
    let ids = sse::ids_of(&events);
    let text = sse::chunks_text(&events);
    report.subscribe_end(
        phase,
        ended.as_str(),
        events.len(),
        &ids,
        sse::type_counts(&events),
        &text,
    );
    (events, ended)
}

pub fn count_event(events: &[SseEvent], name: &str) -> usize {
    events.iter().filter(|e| e.event == name).count()
}

// ---- /metrics 前后快照 diff（性能观测）----

use crate::common::metrics::MetricsSnapshot;
use std::sync::Mutex;
use std::sync::OnceLock;

/// 进程级上次快照（串行场景下 diff 即本场景增量）。
fn metrics_before() -> &'static Mutex<Option<MetricsSnapshot>> {
    static BEFORE: OnceLock<Mutex<Option<MetricsSnapshot>>> = OnceLock::new();
    BEFORE.get_or_init(|| Mutex::new(None))
}

/// 场景收口拉 /metrics 与上次快照 diff 写 jsonl；不可达记 unavailable。
/// assert_hard_all 的 async 上下文调用（Drop 路径不做——异常路径性能数据不重要）。
pub async fn record_metrics_diff(report: &JsonlReporter) {
    let Some(base_url) = &report.base_url else {
        return;
    };
    let Some(now) = MetricsSnapshot::fetch(&report_metrics_client(), base_url).await else {
        report.diagnostic("metrics_diff", "unavailable", "/metrics 不可达或未启用");
        return;
    };
    let prev = metrics_before().lock().expect("metrics lock").take();
    match prev {
        Some(before) => {
            let d = now.diff_json(&before);
            report.diagnostic(
                "metrics_diff",
                &d.to_string(),
                "chat 请求量/延迟增量（http_requests_total / duration histogram）",
            );
        }
        None => {
            report.diagnostic("metrics_diff", "baseline", "本进程首个场景，建立基线快照");
        }
    }
    *metrics_before().lock().expect("metrics lock") = Some(now);
}

fn report_metrics_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .build()
        .expect("metrics client")
}

/// 收尾统一断言：hard 断言有失败时让测试红，报告路径指向 jsonl。
/// finish 前拉 /metrics 快照 diff（性能观测，不影响 verdict）。
pub async fn assert_hard_all(report: JsonlReporter) {
    record_metrics_diff(&report).await;
    let path = report.path.display().to_string();
    assert!(report.finish(), "场景失败：断言明细见 {path}");
}
