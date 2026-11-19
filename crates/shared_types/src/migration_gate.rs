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

/// 标记用户迁移/首建进行中（编排路径调用）。与 `admit_write` 共享同一把
/// 锁——置位后所有新的写准入立即失败。
pub fn begin(user_id: &str) {
    STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .migrating
        .insert(user_id.to_string());
}

/// 清除标记（编排结束——成功/失败路径都必须调）。
pub fn end(user_id: &str) {
    STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .migrating
        .remove(user_id);
}

/// 该用户编排是否进行中（仅观测用；准入判定必须走 admit_write）。
pub fn is_migrating(user_id: &str) -> bool {
    STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .migrating
        .contains(user_id)
}

/// 写准入结果：Ok=已登记在途（所有副本结束时副作用视为完成）；Err=门禁拦截。
pub enum WriteAdmission {
    Admitted(WriteAdmissionGuard),
    Gated,
}

/// 原子写准入：同一把锁内检查编排标记 + 登记在途计数。
pub fn admit_write(user_id: &str) -> WriteAdmission {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if state.migrating.contains(user_id) {
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

    #[test]
    fn admit_after_begin_is_gated_and_drain_sees_registered() {
        let user = "atomic-admit-user";
        begin(user);
        assert!(matches!(admit_write(user), WriteAdmission::Gated));
        end(user);
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
