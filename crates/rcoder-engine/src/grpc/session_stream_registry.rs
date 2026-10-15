//! per-client SSE 转发流注册表（agent_runner 唯一真源架构）。
//!
//! 架构（2026-08-19 去 ring 重构）：**每个 HTTP SSE 客户端一条独立的
//! agent_runner `SubscribeProgress(from_seq)` 订阅**，rcoder 纯转发——
//! 历史回放由 agent_runner 的订阅参数表达（from_seq=0 全量 / N 增量 /
//! u64::MAX live-only），rcoder 不再缓存任何消息（原 SharedStream 的
//! ring/replay/终端即清/fan-out 状态机整体移除）。
//!
//! 本注册表保留两件事：
//! 1. **首连资格**（served_sessions）：无游标客户端"首连兜 chat→SSE 时间差
//!    （from_seq=0 全量回放）vs 中间连接纯实时（不重放，防重复红线）"的裁决；
//!    turn 终态（end_turn/error）时归还资格——新一轮 turn 的首连重新可兜。
//! 2. **活跃流登记**（active）：容器销毁路径（reaper/restart/destroyer）按
//!    grpc_addr / session_id 取消该容器上的所有客户端转发 task。

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use dashmap::DashMap;
use parking_lot::Mutex;
use shared_types::grpc::ProgressEvent;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tonic::Code;
use tracing::info;

/// SSE 共享流关闭回调类型（参数为 grpc_addr）。
/// 容器销毁路径（reaper/restart/ensure/destroyer）按地址关闭前端进度流。
pub type ShutdownSseFn = Arc<dyn Fn(&str) + Send + Sync>;

/// 流错误重试上限（连接失败/流错误的重试次数）
pub(crate) const MAX_RETRIES: u32 = 2;

/// activity 更新节流（秒）：收到 agent 任务进度事件时节流更新 project 活跃时间，
/// 防止 cleanup_task 在长任务执行期间误判 idle。
const ACTIVITY_UPDATE_THROTTLE_SECS: i64 = 10;

/// 活跃 per-client 转发 task 的登记项（Weak：task 自然结束无需回表清理，
/// 失效条目在下次登记/扫描时懒惰回收）
struct ActiveStreamRegistration {
    session_id: String,
    token: std::sync::Weak<CancellationToken>,
}

#[derive(Default)]
struct Admission {
    closed: bool,
    tasks: usize,
}

/// Captured before spawn and held until the forwarding task actually exits.
pub(crate) struct StreamTaskGuard {
    registry: Arc<SessionStreamRegistry>,
    grpc_addr: String,
    token: Arc<CancellationToken>,
}

impl StreamTaskGuard {
    pub(crate) fn token(&self) -> Arc<CancellationToken> {
        Arc::clone(&self.token)
    }
}

impl Drop for StreamTaskGuard {
    fn drop(&mut self) {
        if let Some(mut entries) = self.registry.active.get_mut(&self.grpc_addr) {
            entries.retain(|entry| {
                entry
                    .token
                    .upgrade()
                    .is_some_and(|token| !Arc::ptr_eq(&token, &self.token))
            });
        }
        self.registry
            .active
            .remove_if(&self.grpc_addr, |_, entries| entries.is_empty());
        // DashMap guards are released before taking admission: admit uses the
        // opposite order and never awaits while holding either guard.
        self.registry.admission.lock().tasks -= 1;
        self.registry.task_exited.notify_waiters();
    }
}

/// SSE 流注册表（rcoder 进程级单例，挂在 `AppState`）。
pub struct SessionStreamRegistry {
    /// 已服务过客户端的 session（首连资格）：跨转发 task 生命周期——
    /// 长 turn 中客户端断连重连不会误重放已收消息；终端事件时移除条目
    /// = 新一轮 turn 的首连重新获得资格。
    served_sessions: DashMap<String, ()>,
    /// grpc_addr → 活跃 per-client 转发 task（容器销毁按 addr 批量取消）
    active: DashMap<String, Vec<ActiveStreamRegistration>>,
    admission: Mutex<Admission>,
    shutdown: CancellationToken,
    task_exited: Notify,
}

impl SessionStreamRegistry {
    pub fn new() -> Self {
        Self {
            served_sessions: DashMap::new(),
            active: DashMap::new(),
            admission: Mutex::new(Admission::default()),
            shutdown: CancellationToken::new(),
            task_exited: Notify::new(),
        }
    }

    pub fn is_closed(&self) -> bool {
        self.admission.lock().closed
    }

    /// Admission, ownership and cancellation registration precede spawn. A
    /// concurrent close either rejects this task or owns its cancellation.
    pub(crate) fn admit_stream(
        self: &Arc<Self>,
        grpc_addr: &str,
        session_id: &str,
    ) -> Option<StreamTaskGuard> {
        let mut admission = self.admission.lock();
        if admission.closed {
            return None;
        }
        admission.tasks = admission.tasks.checked_add(1)?;
        let token = Arc::new(self.shutdown.child_token());
        self.register_stream(grpc_addr, session_id, &token);
        Some(StreamTaskGuard {
            registry: Arc::clone(self),
            grpc_addr: grpc_addr.into(),
            token,
        })
    }

    /// Close admission before HTTP drain, then cancel all accepted reads.
    pub fn close(&self) {
        self.admission.lock().closed = true;
        self.shutdown.cancel();
        for entry in self.active.iter() {
            for registration in entry.value() {
                if let Some(token) = registration.token.upgrade() {
                    token.cancel();
                }
            }
        }
    }

    /// Removal from the discovery map is not task-exit evidence. Wait for the
    /// guards of all accepted tasks before allowing storage shutdown.
    pub async fn drain(&self, deadline: tokio::time::Instant) -> anyhow::Result<()> {
        self.close();
        loop {
            let changed = self.task_exited.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.admission.lock().tasks == 0 {
                return Ok(());
            }
            tokio::time::timeout_at(deadline, changed).await
                .map_err(|_| anyhow::anyhow!("SSE drain timed out; forwarding task exit remains unconfirmed, storage remains owned"))?;
        }
    }

    /// 声明首连资格：该 session 第一次被客户端服务返回 true（其订阅
    /// from_seq=0 全量回放，兜 chat→SSE 时间差），后续连接返回 false
    /// （live-only，不重放——防重复红线）。turn 终态自动归还资格。
    pub fn claim_first_client(&self, session_id: &str) -> bool {
        self.served_sessions
            .insert(session_id.to_string(), ())
            .is_none()
    }

    /// turn 终态（end_turn/error）归还首连资格：新一轮 turn 的首个客户端
    /// 重新可兜时间差。转发 task 在终端事件转发后调用。
    pub fn release_first_client_claim(&self, session_id: &str) {
        self.served_sessions.remove_if(session_id, |_, _| true);
    }

    /// 登记活跃转发 task（转发 task 建立时调用；token 由 shutdown 路径取消）。
    /// 懒惰回收：登记时清理同 addr 下已失效（Weak upgrade 失败）的旧条目。
    pub(crate) fn register_stream(
        &self,
        grpc_addr: &str,
        session_id: &str,
        token: &Arc<CancellationToken>,
    ) {
        self.active
            .entry(grpc_addr.to_string())
            .and_modify(|list| {
                list.retain(|r| r.token.upgrade().is_some());
                list.push(ActiveStreamRegistration {
                    session_id: session_id.to_string(),
                    token: Arc::downgrade(token),
                });
            })
            .or_insert_with(|| {
                vec![ActiveStreamRegistration {
                    session_id: session_id.to_string(),
                    token: Arc::downgrade(token),
                }]
            });
        if self.shutdown.is_cancelled() {
            token.cancel();
        }
    }

    /// 强制关闭某 session 的所有客户端转发流（容器销毁/项目删除时调用）。
    /// 全表扫描（活跃 SSE 会话数量级小）；幂等。
    pub fn shutdown_session(&self, session_id: &str) -> bool {
        let mut closed = 0usize;
        for mut entry in self.active.iter_mut() {
            entry.value_mut().retain(|r| {
                r.token.upgrade().is_some_and(|t| {
                    if r.session_id == session_id {
                        t.cancel();
                        closed += 1;
                        false
                    } else {
                        true
                    }
                })
            });
        }
        if closed > 0 {
            info!(
                "[SessionStream] shutdown_session: session_id={}, closed={}",
                session_id, closed
            );
        }
        closed > 0
    }

    /// 按 grpc_addr 批量关闭客户端转发流（容器销毁路径：reaper/restart/ensure/
    /// destroyer 调用——"project/session 记录可能已被清空、只剩 grpc_addr 可用"）。
    /// 幂等：重复调用返回 0。
    pub fn shutdown_streams_by_addr(&self, grpc_addr: &str) -> usize {
        let Some((_, mut list)) = self.active.remove(grpc_addr) else {
            return 0;
        };
        let total = list.len();
        let mut closed = 0usize;
        list.retain(|r| {
            if let Some(t) = r.token.upgrade() {
                t.cancel();
                closed += 1;
                false
            } else {
                false // 失效条目一并清理
            }
        });
        info!(
            "[SessionStream] shutdown_streams_by_addr: grpc_addr={}, matched={}, closed={}",
            grpc_addr, total, closed
        );
        closed
    }

    /// 当前活跃登记的会话数（测试 / 观测用：按 addr 汇总）
    pub fn len(&self) -> usize {
        self.active.iter().map(|e| e.value().len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for SessionStreamRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// turn 边界终端：本轮任务真实结束（正常完成或错误终止）。
/// `cancelled` 不算——它是"用户连发消息自动取消当前任务"的常态事件，agent 随后
/// 继续执行新任务；此刻归还首连资格会破坏下一轮的 replay 语义。
pub(crate) fn is_turn_terminal(message_type: &str, sub_type: &str) -> bool {
    message_type == "SessionPromptEnd" && matches!(sub_type, "end_turn" | "error")
}

/// SSE 流关闭信号：turn 终态（end_turn/error）+ rcoder 内部合成的 `stream_ended`。
/// cancelled 不关流——它是"用户连发消息自动取消"的常态事件，流保持供下一轮
/// 实时投递（与 agent_runner 侧订阅判定对齐）。
pub(crate) fn is_stream_closing(message_type: &str, sub_type: &str) -> bool {
    message_type == "SessionPromptEnd" && matches!(sub_type, "end_turn" | "error" | "stream_ended")
}

pub(crate) fn maybe_update_activity(
    updater: &Arc<dyn Fn(&str) + Send + Sync>,
    session_id: &str,
    last_update_secs: &AtomicI64,
) {
    let now_secs = chrono::Utc::now().timestamp();
    let last = last_update_secs.load(Ordering::Relaxed);
    if now_secs - last < ACTIVITY_UPDATE_THROTTLE_SECS {
        return;
    }
    if last_update_secs
        .compare_exchange(last, now_secs, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    updater(session_id);
}

fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// agent_runner 正常关流但未推终端时的兜底 SessionPromptEnd（seq=0 合成消息）
pub(crate) fn make_prompt_end_event() -> ProgressEvent {
    ProgressEvent {
        message_type: "SessionPromptEnd".to_string(),
        sub_type: "end_turn".to_string(),
        payload: r#"{"reason":"EndTurn","description":"Agent has no task in execution"}"#
            .to_string(),
        request_id: None,
        seq: 0,
        timestamp: now_millis(),
    }
}

/// agent_runner stream failed; keep the captured failure and request identity.
pub(crate) fn make_stream_error_event(code: Code, message: &str) -> ProgressEvent {
    make_stream_error_event_with_identity(code, message, "en-US", "grpc_stream", None)
}

pub(crate) fn make_stream_error_event_with_identity(
    code: Code,
    message: &str,
    locale: &str,
    stage: &str,
    request_id: Option<String>,
) -> ProgressEvent {
    let error_code = map_tonic_code(code);
    let detail = shared_types::ErrorDetail::new(error_code, stage, message).localized(locale);
    let base = shared_types::get_error_message(shared_types::ERR_GRPC_ERROR, locale);
    let payload = serde_json::json!({
        "code": error_code,
        "message": shared_types::sanitize_error_text(&format!("{base}: {}", detail.detail)),
        "error_detail": detail,
    })
    .to_string();
    ProgressEvent {
        message_type: "SessionPromptEnd".to_string(),
        sub_type: "error".to_string(),
        payload,
        request_id,
        seq: 0,
        timestamp: now_millis(),
    }
}

/// Supplemental diagnosis never replaces the original transport failure. It is
/// read only, bounded, and cancellation at the caller can interrupt it.
pub(crate) async fn make_terminal_error_event(
    diag: Option<&Arc<crate::utils::DiagCtx>>,
    locale: &str,
    code: Code,
    message: &str,
    stage: &str,
    request_id: Option<String>,
) -> ProgressEvent {
    let mut event = make_stream_error_event_with_identity(code, message, locale, stage, request_id);
    if let Some(ctx) = diag {
        let observation = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            ctx.runtime
                .diagnose_agent_pod(&ctx.identifier, &ctx.service_type),
        )
        .await;
        if let Ok(Ok(observed)) = observation
            && (observed.has_root_cause() || observed.is_starting_up())
        {
            let observed_reason = crate::utils::root_cause_message(&observed, locale);
            // The original code/message and request identity remain unchanged.
            // Observations describe current container state, not the failed job.
            if let Ok(mut payload) = serde_json::from_str::<serde_json::Value>(&event.payload) {
                payload["observed_container_state"] =
                    serde_json::Value::String(shared_types::sanitize_error_text(&observed_reason));
                event.payload = payload.to_string();
            }
        }
    }
    event
}

/// seq 回退（agent_runner 重启后新 epoch 从 1 重新计数）时的 cursor-reset 哨兵
/// (seq=0):告知客户端重置去重游标,让新 epoch 的低 seq 事件不被静默丢弃。
/// 非终态(message_type≠SessionPromptEnd,不关流)。
pub(crate) fn make_cursor_reset_event() -> ProgressEvent {
    ProgressEvent {
        message_type: "StreamReset".to_string(),
        sub_type: "epoch_changed".to_string(),
        payload: serde_json::json!({
            "reason": "EpochChanged",
            "description": "Agent stream epoch changed; reset your dedup cursor"
        })
        .to_string(),
        request_id: None,
        seq: 0,
        timestamp: now_millis(),
    }
}

pub(crate) fn map_tonic_code(code: Code) -> &'static str {
    match code {
        Code::Unavailable => "GRPC_SERVICE_UNAVAILABLE",
        Code::Cancelled => "GRPC_CANCELLED",
        Code::DeadlineExceeded => "GRPC_DEADLINE_EXCEEDED",
        Code::NotFound => "GRPC_NOT_FOUND",
        Code::PermissionDenied => "GRPC_PERMISSION_DENIED",
        Code::Unauthenticated => "GRPC_UNAUTHENTICATED",
        Code::Internal => "GRPC_INTERNAL",
        Code::ResourceExhausted => "GRPC_RESOURCE_EXHAUSTED",
        _ => "GRPC_UNKNOWN",
    }
}

#[cfg(test)]
mod tests;
