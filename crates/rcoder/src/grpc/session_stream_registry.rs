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
use shared_types::grpc::ProgressEvent;
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

/// SSE 流注册表（rcoder 进程级单例，挂在 `AppState`）。
pub struct SessionStreamRegistry {
    /// 已服务过客户端的 session（首连资格）：跨转发 task 生命周期——
    /// 长 turn 中客户端断连重连不会误重放已收消息；终端事件时移除条目
    /// = 新一轮 turn 的首连重新获得资格。
    served_sessions: DashMap<String, ()>,
    /// grpc_addr → 活跃 per-client 转发 task（容器销毁按 addr 批量取消）
    active: DashMap<String, Vec<ActiveStreamRegistration>>,
}

impl SessionStreamRegistry {
    pub fn new() -> Self {
        Self {
            served_sessions: DashMap::new(),
            active: DashMap::new(),
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
        self.active.is_empty()
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

/// agent_runner 流传输中出错（seq=0 合成消息）
pub(crate) fn make_stream_error_event(code: Code, _message: &str) -> ProgressEvent {
    let error_code = map_tonic_code(code);
    // 用 serde_json 构造,避免 format! 拼接产生非法 JSON。
    let payload = serde_json::json!({
        "code": error_code,
        "message": "Agent execution error, please retry.",
    })
    .to_string();
    ProgressEvent {
        message_type: "SessionPromptEnd".to_string(),
        sub_type: "error".to_string(),
        payload,
        request_id: None,
        seq: 0,
        timestamp: now_millis(),
    }
}

/// gRPC 连接彻底失败(重试耗尽;seq=0 合成终态事件)。
///
/// 有 [`DiagCtx`] 时做一次**实时诊断**(OOM/CrashLoop/容器缺失/启动中),给精准
/// 根因文案;无 DiagCtx(测试 / 无 runtime)→ 通用文案。
pub(crate) async fn make_terminal_error_event(
    diag: Option<&Arc<crate::handler::utils::DiagCtx>>,
    locale: &str,
) -> ProgressEvent {
    use crate::handler::utils::{diagnose, root_cause_message};
    let code = shared_types::error_codes::ERR_AGENT_CONTAINER_UNAVAILABLE;
    let message = match diag {
        Some(ctx) => {
            let d = diagnose(&ctx.runtime, &ctx.identifier, ctx.service_type.clone()).await;
            root_cause_message(&d, locale)
        }
        None => shared_types::error_codes::get_error_message(code, locale),
    };
    let payload = serde_json::json!({
        "code": code,
        "message": message,
    })
    .to_string();
    ProgressEvent {
        message_type: "SessionPromptEnd".to_string(),
        sub_type: "error".to_string(),
        payload,
        request_id: None,
        seq: 0,
        timestamp: now_millis(),
    }
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
