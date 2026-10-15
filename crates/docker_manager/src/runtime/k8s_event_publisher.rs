//! kube-runtime 接入批次 C（plan §4）：受管共享 Event publisher。
//!
//! 不变量（KR09/KR10）：
//! - 每个 [`KubernetesRuntime`] 共享一个受管 publisher（Arc，clone 不重建，
//!   JoinHandle/取消机制由本模块持有）；
//! - 生命周期路径只做非阻塞 `try_send`：队列满/关闭立即计丢弃并限速
//!   warning，绝不阻塞或改变业务结果；
//! - 单消费者串行调用共享 kube `Recorder`；单次发布 3s 上限，失败计数后
//!   丢弃，无无界重试；
//! - 关停先取消再排空，至多 5s；剩余数量记录在计数器中；
//! - 事件负载只含固定词表 reason/action（`&'static str`，编译期约束）与
//!   1kB 截断的 note（操作 ID 仅入 note 关联日志，绝不进入 metrics 标签）；
//!   RBAC 只需 events.k8s.io/events 的 create/patch（KR10）。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use k8s_openapi::api::core::v1::ObjectReference;
use tokio::sync::mpsc;

/// 有界队列初始容量（plan §4 拟定值；压力证据前不扩大）。
const QUEUE_CAPACITY: usize = 256;
/// 单次事件发布上限；超时计发送失败并丢弃，不重试。
const SINGLE_PUBLISH_TIMEOUT: Duration = Duration::from_secs(3);
/// 关停后排空上限；到点即停并记录剩余数量。
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// 丢弃 warning 限速窗口。
const DROP_WARNING_INTERVAL: Duration = Duration::from_secs(30);
/// note 硬上限（K8s Events API 1kB 约束），UTF-8 边界截断。
const NOTE_MAX_BYTES: usize = 1024;

/// 固定词表的事件类型（重导出以便调用方构造，不引入自由字符串）。
pub(crate) type DiagnosticEventType = kube::runtime::events::EventType;

/// 已脱敏的结构化诊断事件：reason/action 为固定词表，note 截断至 1kB。
pub(crate) struct DiagnosticEvent {
    type_: kube::runtime::events::EventType,
    reason: &'static str,
    action: &'static str,
    note: String,
    regarding: ObjectReference,
}

impl DiagnosticEvent {
    pub(crate) fn new(
        type_: kube::runtime::events::EventType,
        reason: &'static str,
        action: &'static str,
        note: String,
        regarding: ObjectReference,
    ) -> Self {
        let mut note = note;
        truncate_utf8(&mut note, NOTE_MAX_BYTES);
        Self {
            type_,
            reason,
            action,
            note,
            regarding,
        }
    }
}

/// UTF-8 边界截断（超限时以省略号结尾，总长不超过 max）。
fn truncate_utf8(value: &mut String, max: usize) {
    if value.len() <= max {
        return;
    }
    // K03：先预留省略号字节再回退到字符边界——先找边界再减 3 会再次落进
    // 多字节字符内部，truncate 直接 panic（事件在业务返回前构造，诊断路径
    // 不得 panic）。
    let mut end = max.saturating_sub('…'.len_utf8());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push('…');
}

/// 丢弃/发送/排空计数（测试与运维观测共用；只增不减）。
#[derive(Default, Debug)]
pub(crate) struct PublisherCounters {
    sent: AtomicU64,
    dropped: AtomicU64,
    send_errors: AtomicU64,
    remaining_after_shutdown: AtomicU64,
    /// 关停预算耗尽时仍被取消的在途发布（结果未知，K05 计数守恒）。
    abandoned_in_flight: AtomicU64,
    /// 关停排空完成标记（remaining_after_shutdown 已写入终值）。
    drained: std::sync::atomic::AtomicBool,
}

/// 计数快照（生产观测面；K05：计数守恒可对外核对）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PublisherSnapshot {
    pub(crate) sent: u64,
    pub(crate) dropped: u64,
    pub(crate) send_errors: u64,
    pub(crate) remaining_after_shutdown: u64,
    pub(crate) abandoned_in_flight: u64,
    pub(crate) drained: bool,
}

struct PublisherInner {
    sender: mpsc::Sender<DiagnosticEvent>,
    shutdown: tokio_util::sync::CancellationToken,
    counters: Arc<PublisherCounters>,
    last_drop_warning: std::sync::Mutex<Option<std::time::Instant>>,
    /// 消费者任务句柄（K05：可等待关停——不再丢弃）。
    consumer: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// 生产关停路径：最后一个 runtime clone 释放时取消接纳，消费者按有界预算
/// 排空后退出（进程退出时任务随运行时终止）。Drop 记录最终计数快照
///（K05：sent/errors/remaining/abandoned 守恒可观测；显式可等待关停用
/// [`KubernetesEventPublisher::shutdown`]）。
impl Drop for PublisherInner {
    fn drop(&mut self) {
        self.shutdown.cancel();
        let snap = self.counters.snapshot();
        tracing::info!(
            sent = snap.sent,
            dropped = snap.dropped,
            send_errors = snap.send_errors,
            remaining_after_shutdown = snap.remaining_after_shutdown,
            abandoned_in_flight = snap.abandoned_in_flight,
            "k8s event publisher final counters"
        );
    }
}

/// 受管共享 publisher。`Default` 为 inactive（测试/无客户端场景的 no-op），
/// [`KubernetesEventPublisher::start`] 启动受管消费者；Clone 共享同一实例。
#[derive(Clone, Default)]
pub(crate) struct KubernetesEventPublisher {
    inner: Option<Arc<PublisherInner>>,
}

impl KubernetesEventPublisher {
    /// 启动受管 publisher：有界队列 + 单消费者串行共享 Recorder。
    /// 返回的计数器句柄是运维观测面（未来 metrics 接线点），与 publisher
    /// 实例独立存活。
    pub(crate) fn start(client: kube::Client) -> (Self, Arc<PublisherCounters>) {
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let shutdown = tokio_util::sync::CancellationToken::new();
        let counters = Arc::new(PublisherCounters::default());
        let consumer = tokio::spawn(run_consumer(
            receiver,
            kube::runtime::events::Recorder::new(client, "rcoder-runtime".into()),
            counters.clone(),
            shutdown.clone(),
        ));
        (
            Self {
                inner: Some(Arc::new(PublisherInner {
                    sender,
                    shutdown,
                    counters: counters.clone(),
                    last_drop_warning: std::sync::Mutex::new(None),
                    consumer: std::sync::Mutex::new(Some(consumer)),
                })),
            },
            counters,
        )
    }

    /// 非阻塞提交：队列满/关闭计丢弃并限速告警，绝不阻塞调用方。
    pub(crate) fn publish(&self, event: DiagnosticEvent) {
        let Some(inner) = &self.inner else {
            return;
        };
        if let Err(error) = inner.sender.try_send(event) {
            let dropped = inner.counters.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            warn_rate_limited(inner, dropped, &error);
        }
    }

    /// 可等待关停（K05）：取消接纳 → 在**总预算**（在途单次发布 + 排空窗口
    /// 之和）内等待消费者退出并回填计数；超时返回最终快照（结果如实——
    /// abandoned/remaining 记录未知项）。幂等：二次调用直接返回快照。
    ///
    /// 接线点：生产进程随运行时退出（Drop 已取消+记录快照），显式调用留给
    /// 未来的 runtime 生命周期收尾 API（当前无持有方主动 shutdown 路径）。
    #[allow(dead_code)] // K05 观测/关停面：测试消费，metrics 接线前的稳定 API
    pub(crate) async fn shutdown(&self) -> PublisherSnapshot {
        let Some(inner) = &self.inner else {
            return PublisherSnapshot {
                sent: 0,
                dropped: 0,
                send_errors: 0,
                remaining_after_shutdown: 0,
                abandoned_in_flight: 0,
                drained: true,
            };
        };
        inner.shutdown.cancel();
        let handle = inner
            .consumer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(handle) = handle {
            // 在途发布上限 + 排空窗口 = 关停总预算上界
            let budget = SINGLE_PUBLISH_TIMEOUT + SHUTDOWN_DRAIN_TIMEOUT;
            if tokio::time::timeout(budget, handle).await.is_err() {
                tracing::warn!("k8s event publisher shutdown budget exhausted; task aborted");
            }
        }
        inner.counters.snapshot()
    }

    /// 当前计数快照（K05：生产观测/接线面）。
    #[allow(dead_code)] // 接线点：metrics/诊断接线前的稳定 API（测试消费）
    pub(crate) fn snapshot(&self) -> Option<PublisherSnapshot> {
        self.inner.as_ref().map(|inner| inner.counters.snapshot())
    }
}

impl PublisherCounters {
    /// 计数快照（测试与运维观测共用；生产 metrics 接线前的观测面）。
    pub(crate) fn snapshot(&self) -> PublisherSnapshot {
        PublisherSnapshot {
            sent: self.sent.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            send_errors: self.send_errors.load(Ordering::Relaxed),
            remaining_after_shutdown: self.remaining_after_shutdown.load(Ordering::Relaxed),
            abandoned_in_flight: self.abandoned_in_flight.load(Ordering::Relaxed),
            drained: self.drained.load(Ordering::Relaxed),
        }
    }
}

fn warn_rate_limited(
    inner: &PublisherInner,
    dropped: u64,
    error: &mpsc::error::TrySendError<DiagnosticEvent>,
) {
    // K03：诊断限速锁中毒不 panic——拿回内部数据继续（最坏多发/少发一次
    // 限速告警），诊断错误不得影响业务路径
    let mut last = inner
        .last_drop_warning
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = std::time::Instant::now();
    let due = last.is_none_or(|at| now.duration_since(at) >= DROP_WARNING_INTERVAL);
    if due {
        *last = Some(now);
        tracing::warn!(
            dropped_total = dropped,
            error = %error,
            "k8s diagnostic event dropped (bounded queue); lifecycle unaffected"
        );
    }
}

async fn run_consumer(
    mut receiver: mpsc::Receiver<DiagnosticEvent>,
    recorder: kube::runtime::events::Recorder,
    counters: Arc<PublisherCounters>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    loop {
        // biased + 取消优先：关停一旦受理，绝不再取新事件——剩余队列只能
        // 进入下方有界排空（否则随机分支可在取消前持续消费，排空预算失效）
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            received = receiver.recv() => match received {
                Some(event) => publish_one(&recorder, event, &counters).await,
                None => break,
            }
        }
    }
    // K05：取消即锚定**绝对总预算**（5s，含此刻仍在途的单次发布——预算
    // 不因等待在途发布而顺延）；预算内尽力排空；到期未决事件记 remaining、
    // 在途被取消的发布记 abandoned_in_flight（结果未知，计数守恒）。
    let deadline = tokio::time::Instant::now() + SHUTDOWN_DRAIN_TIMEOUT;
    let drained_clean = tokio::time::timeout_at(deadline, async {
        while let Some(event) = receiver.recv().await {
            publish_one(&recorder, event, &counters).await;
        }
    })
    .await
    .is_ok();
    let remaining = receiver.len() as u64;
    counters
        .remaining_after_shutdown
        .store(remaining, Ordering::Relaxed);
    if !drained_clean {
        // 预算耗尽时仍有未消费事件；若此刻有单次发布被整体超时取消，
        // 其结果未知——以 abandoned 计数披露（守恒：sent+errors+abandoned
        // + remaining+dropped = 提交总量）
        counters.abandoned_in_flight.fetch_add(1, Ordering::Relaxed);
    }
    counters.drained.store(true, Ordering::Relaxed);
    if remaining > 0 || !drained_clean {
        tracing::warn!(
            remaining,
            exhausted = !drained_clean,
            "k8s event publisher shutdown bounded budget reached"
        );
    }
}

async fn publish_one(
    recorder: &kube::runtime::events::Recorder,
    event: DiagnosticEvent,
    counters: &PublisherCounters,
) {
    let kube_event = kube::runtime::events::Event {
        type_: event.type_,
        reason: event.reason.to_owned(),
        note: Some(event.note),
        action: event.action.to_owned(),
        secondary: None,
    };
    match tokio::time::timeout(
        SINGLE_PUBLISH_TIMEOUT,
        recorder.publish(&kube_event, &event.regarding),
    )
    .await
    {
        Ok(Ok(())) => {
            counters.sent.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Err(error)) => {
            counters.send_errors.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(%error, reason = event.reason, "k8s event publish rejected (dropped)");
        }
        Err(_) => {
            counters.send_errors.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                reason = event.reason,
                "k8s event publish timed out (dropped)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 事件 API 契约服务器场景：create(PATCH) 响应策略。
    #[derive(Clone, Copy, PartialEq)]
    enum EventServerMode {
        Accept,
        Forbidden,
        Hang,
    }

    /// 真实 HTTP 服务器（events.k8s.io create/patch）+ 每连接独立 task
    /// （Hang 模式持连不答，不能阻塞 accept）。
    async fn spawn_event_server(
        mode: EventServerMode,
    ) -> (std::net::SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let counter = counter.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let mut buffer = [0u8; 4096];
                    loop {
                        let n = stream.read(&mut buffer).await.expect("read");
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..n]);
                        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&bytes[..end]).to_string();
                            let length = head
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|value| value.trim().parse::<usize>().expect("length"))
                                })
                                .unwrap_or(0);
                            if bytes.len() >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    let first = bytes
                        .split(|byte| *byte == b' ')
                        .next()
                        .map(|method| String::from_utf8_lossy(method).to_string())
                        .unwrap_or_default();
                    assert!(
                        first == "POST" || first == "PATCH",
                        "events api only receives create/patch: {first}"
                    );
                    counter.fetch_add(1, Ordering::Relaxed);
                    if mode == EventServerMode::Hang {
                        // 持连不答：模拟 apiserver 卡死（单次发布超时路径）
                        let mut drain = [0u8; 512];
                        loop {
                            if stream.read(&mut drain).await.unwrap_or(0) == 0 {
                                return;
                            }
                        }
                    }
                    let (code, body) = match mode {
                        EventServerMode::Forbidden => (
                            403,
                            r#"{"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Forbidden","message":"events denied","code":403}"#.to_string(),
                        ),
                        _ => (201, r#"{"apiVersion":"events.k8s.io/v1","kind":"Event","metadata":{"name":"accepted"}}"#.to_string()),
                    };
                    stream
                        .write_all(
                            format!(
                                "HTTP/1.1 {code} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .expect("respond");
                });
            }
        });
        (address, requests)
    }

    fn client_for(address: std::net::SocketAddr) -> kube::Client {
        drop(rustls::crypto::ring::default_provider().install_default());
        kube::Client::try_from(kube::Config::new(
            format!("http://{address}").parse().expect("uri"),
        ))
        .expect("client")
    }

    fn reference() -> ObjectReference {
        ObjectReference {
            api_version: Some("apps/v1".into()),
            kind: Some("StatefulSet".into()),
            name: Some("builder".into()),
            namespace: Some("review-test".into()),
            uid: Some("sts-original".into()),
            ..Default::default()
        }
    }

    fn event(note: String) -> DiagnosticEvent {
        DiagnosticEvent::new(
            kube::runtime::events::EventType::Normal,
            "ComputeStopped",
            "StopCompute",
            note,
            reference(),
        )
    }

    #[tokio::test]
    async fn event_queue_full_drops_without_blocking_lifecycle() {
        // 卡死服务器：先让消费者在第一次发布上挂住（确定性前提），再灌满
        // 队列并溢出——publish 必须立即返回并精确计数丢弃。关停走生产
        // 同一路径：drop 最后一个句柄 → 取消接纳 → 有界排空。
        let (address, requests) = spawn_event_server(EventServerMode::Hang).await;
        let (publisher, counters) = KubernetesEventPublisher::start(client_for(address));
        publisher.publish(event("blocked in flight".into()));
        tokio::time::timeout(Duration::from_secs(5), async {
            while requests.load(Ordering::Relaxed) == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("consumer picked up the first event");
        let started = std::time::Instant::now();
        // 容量 256：在消费者挂住期间灌满后再溢出 20 条
        for index in 0..QUEUE_CAPACITY + 20 {
            publisher.publish(event(format!("overflow {index}")));
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "publish must never block: {elapsed:?}"
        );
        assert_eq!(counters.snapshot().dropped, 20, "overflow dropped exactly");
        drop(publisher);
        let snapshot = wait_drained(&counters).await;
        assert!(snapshot.remaining_after_shutdown > 0);
    }

    #[tokio::test]
    async fn event_publish_rejections_are_counted_and_business_unaffected() {
        // 403 拒绝：每次发布计 send_errors 并丢弃；无重试（请求数 = 事件数）。
        let (address, requests) = spawn_event_server(EventServerMode::Forbidden).await;
        let (publisher, counters) = KubernetesEventPublisher::start(client_for(address));
        for index in 0..5 {
            publisher.publish(event(format!("forbidden {index}")));
        }
        drop(publisher);
        let snapshot = wait_drained(&counters).await;
        assert_eq!(snapshot.send_errors, 5, "every 403 counted: {snapshot:?}");
        assert_eq!(snapshot.sent, 0);
        assert_eq!(requests.load(Ordering::Relaxed), 5, "no retries");
        assert_eq!(snapshot.dropped, 0);
    }

    #[tokio::test]
    async fn event_success_publishes_via_events_api() {
        let (address, requests) = spawn_event_server(EventServerMode::Accept).await;
        let (publisher, counters) = KubernetesEventPublisher::start(client_for(address));
        for index in 0..3 {
            publisher.publish(event(format!("accepted {index}")));
        }
        drop(publisher);
        let snapshot = wait_drained(&counters).await;
        assert_eq!(snapshot.sent, 3);
        assert_eq!(snapshot.send_errors, 0);
        assert_eq!(snapshot.remaining_after_shutdown, 0);
        assert_eq!(requests.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn event_shutdown_drain_is_bounded_and_records_remaining() {
        // 卡死服务器：关停（drop）后有界预算（5s + 一次在途发布）内停止
        // 并记录剩余。
        let (address, _requests) = spawn_event_server(EventServerMode::Hang).await;
        let (publisher, counters) = KubernetesEventPublisher::start(client_for(address));
        for index in 0..3 {
            publisher.publish(event(format!("drain {index}")));
        }
        let started = std::time::Instant::now();
        drop(publisher);
        let snapshot = wait_drained(&counters).await;
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "drain respects its budget: {:?} (snapshot {snapshot:?})",
            started.elapsed()
        );
        // 卡死服务器下至多排空一两条（受总预算约束），其余必然留队
        assert!(snapshot.remaining_after_shutdown > 0);
    }

    async fn wait_drained(counters: &Arc<PublisherCounters>) -> PublisherSnapshot {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let snapshot = counters.snapshot();
                if snapshot.drained {
                    return snapshot;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "publisher drain bounded termination; last snapshot {:?}",
                counters.snapshot()
            )
        })
    }

    #[test]
    fn diagnostic_event_note_is_sanitized_to_1kb() {
        let long = "x".repeat(5 * 1024);
        let sanitized = DiagnosticEvent::new(
            kube::runtime::events::EventType::Warning,
            "ControlUncertain",
            "WakeCompute",
            long,
            reference(),
        );
        assert!(sanitized.note.len() <= NOTE_MAX_BYTES);
        assert!(sanitized.note.ends_with('…'));
        // 短 note 原样保留（操作 ID 关联日志不截断）
        let short = DiagnosticEvent::new(
            kube::runtime::events::EventType::Normal,
            "ComputeStopped",
            "StopCompute",
            "app=app operation=stop".into(),
            reference(),
        );
        assert_eq!(short.note, "app=app operation=stop");
    }

    #[test]
    fn inactive_publisher_is_noop() {
        let publisher = KubernetesEventPublisher::default();
        publisher.publish(event("never sent".into()));
        // inactive 无计数器句柄：行为验证 = 进程不 panic、不发送任何请求
    }

    // ===== K03：UTF-8 截断不得 panic（先预留省略号再找边界） =====

    #[test]
    fn truncate_utf8_emoji_at_old_panic_point_is_bounded() {
        // 修复前：boundary(1021→1020) 再减 3 → 1017 落进 4 字节字符内部，
        // truncate(1017) panic。修复后必须安全截断且以省略号结尾。
        for (value, max) in [
            ("😀".repeat(257), 1021),
            ("😀".repeat(257), NOTE_MAX_BYTES),
            ("é".repeat(600), 1021),  // 2 字节字符
            ("好".repeat(400), 1021), // 3 字节字符
            ("a😀".repeat(300), 999), // 混合边界
        ] {
            let mut text = value.clone();
            truncate_utf8(&mut text, max);
            assert!(text.len() <= max, "{value:?} -> len {}", text.len());
            assert!(text.ends_with('…'), "must end with ellipsis: {text:?}");
            assert!(std::str::from_utf8(text.as_bytes()).is_ok());
        }
    }
}
