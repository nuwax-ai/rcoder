//! 迁移写门禁（cutover/首建共用，第五轮复审后下移 shared_types）。
//!
//! 三个使用方：
//! - `rcoder-engine` intercept：写请求原子准入（检查+登记同锁），guard 经
//!   request extension 传递给 handler（file-server 的 spawn_blocking 写
//!   线程持有至副作用结束）+ 转发路径注入响应流。
//! - `rcoder-engine` orchestrate：cutover begin → 排空在途 → 停 agent →
//!   最终同步 → 翻标记 → end。
//! - `docker_manager` k8s_agent_create：新用户首建/续接——begin 关闭新写
//!   → 排空 → **排空后重新探测**共享目录（受理中的写未建目录时盲探测会
//!   误判 Absent）→ 决策 → end。
//!
//! 原子边界：admit_write 与 begin 共享同一把短锁——begin 置位后新的
//! 准入必然失败，drain 归零即真正的排空边界。
//!
//! 范围：进程内（单副本）。多副本需共享门禁态（与 TTL 判据同批的生产
//! 前置）。

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use axum::http::Method;

#[derive(Default)]
struct GateState {
    /// 门禁按持有者 token 记账（第十二轮 P1：HashSet 语义下任一 guard 的
    /// Drop 都会清掉整个用户标志——重试请求的 guard 会提前放行旧后台任务
    /// 仍持有的门禁）。token 匹配才允许解除。
    migrating: HashMap<String, u64>,
    inflight: HashMap<String, usize>,
}

fn next_gate_token() -> u64 {
    static TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    TOKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

static STATE: LazyLock<std::sync::Mutex<GateState>> =
    LazyLock::new(|| std::sync::Mutex::new(GateState::default()));

static DRAINED: LazyLock<tokio::sync::Notify> = LazyLock::new(tokio::sync::Notify::new);

/// 标记用户迁移/首建进行中（编排路径调用）。与 `admit_write` 共享同一把
/// 锁——置位后所有新的写准入立即失败。
/// 登记（已存在时**不覆盖**并返回 None——insert 覆盖会让旧持有者的
/// Drop 解除新门禁，第十二轮 P1）。调用方对 None 按编排冲突处理。
pub fn begin(user_id: &str) -> Option<u64> {
    let token = next_gate_token();
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    match state.migrating.entry(user_id.to_string()) {
        std::collections::hash_map::Entry::Vacant(slot) => {
            slot.insert(token);
            Some(token)
        }
        std::collections::hash_map::Entry::Occupied(_) => None,
    }
}

/// 清除标记（编排结束——成功/失败路径都必须调）。
/// 按持有者 token 解除（不匹配时无操作——别的持有者仍在门禁内）。
pub fn end_with_token(user_id: &str, token: u64) {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.migrating.get(user_id).copied() == Some(token) {
        state.migrating.remove(user_id);
    }
}

/// 该用户编排是否进行中（仅观测用；准入判定必须走 admit_write）。
pub fn is_migrating(user_id: &str) -> bool {
    STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .migrating
        .contains_key(user_id)
}

/// 写准入结果：Ok=已登记在途（所有副本结束时副作用视为完成）；Err=门禁拦截。
pub enum WriteAdmission {
    Admitted(WriteAdmissionGuard),
    Gated,
}

/// 原子写准入：同一把锁内检查编排标记 + 登记在途计数。
pub fn admit_write(user_id: &str) -> WriteAdmission {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.migrating.contains_key(user_id) {
        return WriteAdmission::Gated;
    }
    let count = state.inflight.entry(user_id.to_string()).or_insert(0);
    *count += 1;
    WriteAdmission::Admitted(WriteAdmissionGuard(Arc::new(user_id.to_string())))
}

/// 在途写凭据：`Clone` 语义 = 多个持有方（HTTP 响应流 / handler 的
/// spawn_blocking 写线程）各持一份，**全部释放**时计数才归零——排空
/// 边界覆盖实际副作用结束（第五轮 P1：HTTP 请求取消不中止已启动的
/// 同步写，凭据必须由写任务持有）。
pub struct WriteAdmissionGuard(Arc<String>);

impl std::fmt::Debug for WriteAdmissionGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteAdmissionGuard")
            .field("user_id", &self.0.as_str())
            .finish()
    }
}

/// 计数与 guard **实例数**同步：每个实例 drop 都 -1（Clone 时 +1）——
/// HTTP 层取消（原始 guard drop）后写任务持有的 clone 仍在途，两份都
/// 释放计数才归零。
impl Drop for WriteAdmissionGuard {
    fn drop(&mut self) {
        let emptied = {
            let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
            match state.inflight.get_mut(self.0.as_str()) {
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

impl Clone for WriteAdmissionGuard {
    fn clone(&self) -> Self {
        let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        let count = state.inflight.entry((**self.0).to_string()).or_insert(0);
        *count += 1;
        WriteAdmissionGuard(Arc::clone(&self.0))
    }
}

/// 等待该用户在途写归零（begin 之后调用）。返回超时后仍在途的笔数。
/// begin 已在锁内置位——此后 admit_write 不可能再成功登记，归零即真正
/// 的排空边界。通知-轮询混合（Notify 唤醒 + 50ms 兜底防漏唤醒竞态）。
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
        let token = begin("gate-test-user").expect("first begin");
        assert!(is_migrating("gate-test-user"));
        end_with_token("gate-test-user", token);
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

    #[test]
    fn admit_after_begin_is_gated_and_drain_sees_registered() {
        let user = "atomic-admit-user";
        let token = begin(user).expect("begin");
        assert!(matches!(admit_write(user), WriteAdmission::Gated));
        end_with_token(user, token);
        let guard = match admit_write(user) {
            WriteAdmission::Admitted(guard) => guard,
            WriteAdmission::Gated => panic!("admission must succeed after end"),
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let remaining = rt.block_on(async {
            drain_user_writes(user, std::time::Duration::from_millis(120)).await
        });
        assert_eq!(remaining, 1, "在途写必须被 drain 观测到");
        drop(guard);
        let remaining = rt.block_on(async {
            drain_user_writes(user, std::time::Duration::from_millis(200)).await
        });
        assert_eq!(remaining, 0, "guard 释放后 drain 归零");
    }

    /// 克隆语义（第五轮 P1）：HTTP 层取消（drop 原始 guard）后，写任务
    /// 持有的 clone 仍在途——计数只有两份都释放才归零。
    #[test]
    fn cloned_guard_keeps_write_inflight_until_task_releases() {
        let user = "clone-lifecycle-user";
        let original = match admit_write(user) {
            WriteAdmission::Admitted(guard) => guard,
            WriteAdmission::Gated => panic!("admission must succeed"),
        };
        let task_copy = original.clone();
        drop(original); // HTTP 请求取消
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let remaining = rt.block_on(async {
            drain_user_writes(user, std::time::Duration::from_millis(120)).await
        });
        assert_eq!(remaining, 1, "写任务持有的 clone 必须保持计数");
        drop(task_copy); // spawn_blocking 写完
        let remaining = rt.block_on(async {
            drain_user_writes(user, std::time::Duration::from_millis(200)).await
        });
        assert_eq!(remaining, 0);
    }
}

/// RAII 编排门禁（第七轮复审 P1）：`begin_guarded` 置位并返回本 guard，
/// Drop 时自动 `end`——编排 future 在任意 `await` 点被取消（drop）时也会
/// 释放，杜绝手写 `end` 遗漏导致的该用户写准入永久冻结。
pub struct MigrationGuard {
    user_id: String,
    token: u64,
}

impl Drop for MigrationGuard {
    fn drop(&mut self) {
        end_with_token(&self.user_id, self.token);
    }
}

/// 置位并返回 RAII guard。**同用户已有门禁时返回 None**（旧后台任务仍在
/// 切换中——重试请求不得并发做 fresh-init 决策，调用方应走共享路径放弃
/// 本次编排，而非叠加第二个门禁）。
pub fn begin_guarded(user_id: &str) -> Option<MigrationGuard> {
    let token = begin(user_id)?;
    Some(MigrationGuard {
        user_id: user_id.to_string(),
        token,
    })
}

#[cfg(test)]
mod cancel_safety_tests {
    use super::*;

    /// 第七轮 P1 反例：编排 future 在 begin 后被取消（guard 随 future drop）
    /// ——写准入必须恢复，不得永久 Gated。
    #[test]
    fn cancelled_choreography_releases_gate() {
        let user = "cancel-safety-user";
        {
            let _guard = begin_guarded(user).expect("first acquisition");
            assert!(is_migrating(user));
            // 模拟编排 future 在 await 点被取消：guard 随作用域 drop。
        }
        assert!(
            !is_migrating(user),
            "guard drop 后必须解除门禁——否则该用户写永久 503"
        );
        // 取消后写准入恢复
        assert!(matches!(
            admit_write(user),
            WriteAdmission::Admitted(_)
        ));
    }
}

/// 第十二轮 P1 核心反例：A 后台持门禁 → B 重试 begin 被拒（None）→
/// B 的任何 drop 不得清除 A 的门禁。
#[test]
fn retry_guard_cannot_release_older_holder_gate() {
    let user = "gate-overlap-user";
    let guard_a = begin_guarded(user).expect("A acquires");
    // B 重试：门禁被 A 持有 → 拒绝并发 fresh-init
    assert!(begin_guarded(user).is_none(), "B must be rejected while A holds");
    // 裸 begin 撞已有持有者：不覆盖、返回 None（旧实现 insert 覆盖会让
    // B 的 Drop 解除 A 的门禁——测试在旧实现上曾红）。
    assert!(begin(user).is_none(), "裸 begin 撞已有必须拒绝且不覆盖");
    drop(guard_a);
    assert!(!is_migrating(user));
}
