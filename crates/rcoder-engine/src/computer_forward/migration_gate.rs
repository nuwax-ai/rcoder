//! cutover 写门禁（终审 F1 + 复审 P1 原子化）。
//!
//! 缺口：cutover 持锁期间（停 agent → 增量 sync → 翻标记），该用户仍
//! NotMigrated——文件写请求经 intercept 放行本地，rcoder 直接写共享卷；
//! 增量 sync 之后、翻标记之前受理的写入返回成功但不在 RBD（丢写）。
//! 迁移锁只互斥编排/ensure，不覆盖文件路由，"停 agent=数据面冻结"不成立。
//!
//! 门禁：orchestrate 的 cutover 在持锁后标记该用户 migrating，翻标记后
//! （成功/失败路径均）清除；intercept 对**写方法**（POST/PUT/DELETE/PATCH）
//! 命中透传清单的请求走 `admit_write`——migrating 中一律 503（调用方重试
//! 在切换完成后成功）。读操作不拦（旧卷读不影响一致性，切换后读新卷）。
//!
//! 原子边界（复审第四轮 P1）：`is_migrating` 检查与在途计数登记曾是两个
//! 操作，存在「检查通过 → begin → drain 归零 → 迟到登记 → 旧卷写入」的
//! 确定性交错。现在两者合并进**同一把短锁**：begin 持锁置位后，
//! admit_write 在锁内看到 migrating 即拒绝——此后不可能有新登记，drain
//! 归零即达成了真正的排空边界。
//!
//! 在途写的生命周期：guard 的 Drop 时机 = 副作用真正结束。intercept 把
//! guard 注入响应 body 流（流终结/放弃时释放），而非 middleware 返回时——
//! 流式响应（大文件回读/SSE）的正文发送期间写仍算在途。
//!
//! 范围：进程内（单副本）。多副本需共享门禁态（与 TTL 判据同批的生产
//! 前置，README §10 已披露多副本限制）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use axum::http::Method;

#[derive(Default)]
struct GateState {
    migrating: HashSet<String>,
    inflight: HashMap<String, usize>,
}

static STATE: LazyLock<std::sync::Mutex<GateState>> =
    LazyLock::new(|| std::sync::Mutex::new(GateState::default()));

static DRAINED: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);

/// 标记用户迁移进行中（cutover 持锁后调用）。
/// 与 `admit_write` 共享同一把锁——置位后所有新的写准入立即失败。
pub fn begin(user_id: &str) {
    STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .migrating
        .insert(user_id.to_string());
}

/// 清除标记（cutover 结束——成功/失败路径都必须调；orchestrate 用显式
/// 调用而非 Drop guard，确保失败信息打完后再放行）。
pub fn end(user_id: &str) {
    STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .migrating
        .remove(user_id);
}

/// 该用户迁移是否进行中（仅观测/日志用；准入判定必须走 admit_write）。
pub fn is_migrating(user_id: &str) -> bool {
    STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .migrating
        .contains(user_id)
}

/// 写准入结果：Ok=已登记在途（guard 释放即副作用结束）；Err=门禁拦截。
pub enum WriteAdmission {
    Admitted(WriteAdmissionGuard),
    Gated,
}

/// 原子写准入：同一把锁内检查 migrating + 登记在途计数。
/// 返回 Err(Gated) 时调用方应回 503。
pub fn admit_write(user_id: &str) -> WriteAdmission {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.migrating.contains(user_id) {
        return WriteAdmission::Gated;
    }
    let count = state.inflight.entry(user_id.to_string()).or_insert(0);
    *count += 1;
    WriteAdmission::Admitted(WriteAdmissionGuard {
        user_id: user_id.to_string(),
    })
}

/// 在途写登记 guard：Drop 时计数 -1 并唤醒排空等待。
pub struct WriteAdmissionGuard {
    user_id: String,
}

impl Drop for WriteAdmissionGuard {
    fn drop(&mut self) {
        let emptied = {
            let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
            match state.inflight.get_mut(&self.user_id) {
                Some(count) => {
                    *count = count.saturating_sub(1);
                    *count == 0
                }
                None => true,
            }
        };
        if emptied {
            DRAINED.notify_waiters();
        }
    }
}

/// 等待该用户在途写归零（cutover begin 后、停 agent 前调用）。
/// 返回超时后仍在途的笔数（0=干净排空）。begin 已在锁内置位——此后
/// admit_write 不可能再成功登记，归零即真正的排空边界。
/// 通知-轮询混合：Notify 唤醒 + 50ms 轮询兜底（防漏唤醒竞态）。
pub async fn drain_user_writes(user_id: &str, timeout: std::time::Duration) -> usize {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .inflight
            .get(user_id)
            .copied()
            .unwrap_or(0);
        if remaining == 0 || tokio::time::Instant::now() >= deadline {
            return remaining;
        }
        let wake_at = std::cmp::min(
            deadline,
            tokio::time::Instant::now() + std::time::Duration::from_millis(50),
        );
        tokio::select! {
            () = DRAINED.notified() => {}
            () = tokio::time::sleep_until(wake_at) => {}
        }
    }
}

/// 请求是否为写方法（门禁只拦写）。
pub fn is_write_method(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::DELETE | Method::PATCH
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_begin_end_cycle() {
        begin("gate-test-user");
        assert!(is_migrating("gate-test-user"));
        end("gate-test-user");
        assert!(!is_migrating("gate-test-user"));
    }

    #[test]
    fn write_method_classification() {
        assert!(is_write_method(&Method::POST));
        assert!(is_write_method(&Method::PUT));
        assert!(is_write_method(&Method::DELETE));
        assert!(is_write_method(&Method::PATCH));
        assert!(!is_write_method(&Method::GET));
        assert!(!is_write_method(&Method::HEAD));
    }

    /// 复审第四轮 P1 的核心反例：begin 后准入必须立即被拒（原子边界），
    /// 不存在「检查通过后 begin 置位、随后仍登记成功」的窗口。
    #[test]
    fn admit_after_begin_is_gated_and_drain_sees_registered() {
        let user = "atomic-admit-user";
        begin(user);
        assert!(matches!(admit_write(user), WriteAdmission::Gated));
        end(user);
        // 门禁关闭后准入成功，drain 能看到登记中的写
        let guard = match admit_write(user) {
            WriteAdmission::Admitted(guard) => guard,
            WriteAdmission::Gated => panic!("admission must succeed after end"),
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let remaining = rt.block_on(async {
            // guard 仍在作用域内 → drain 超时后应报告 1 笔在途
            drain_user_writes(user, std::time::Duration::from_millis(120)).await
        });
        assert_eq!(remaining, 1, "在途写必须被 drain 观测到");
        drop(guard);
        let remaining = rt.block_on(async {
            drain_user_writes(user, std::time::Duration::from_millis(200)).await
        });
        assert_eq!(remaining, 0, "guard 释放后 drain 归零");
    }
}
