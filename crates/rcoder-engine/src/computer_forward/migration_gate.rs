//! cutover 写门禁（终审 F1）。
//!
//! 缺口：cutover 持锁期间（停 agent → 增量 sync → 翻标记），该用户仍
//! NotMigrated——文件写请求经 intercept 放行本地，rcoder 直接写共享卷；
//! 增量 sync 之后、翻标记之前受理的写入返回成功但不在 RBD（丢写）。
//! 迁移锁只互斥编排/ensure，不覆盖文件路由，"停 agent=数据面冻结"不成立。
//!
//! 门禁：orchestrate 的 cutover 在持锁后标记该用户 migrating，翻标记后
//! （成功/失败路径均）清除；intercept 对**写方法**（POST/PUT/DELETE/PATCH）
//! 命中透传清单的请求，先查门禁——migrating 中一律 503（调用方重试在切换
//! 完成后成功）。读操作不拦（旧卷读不影响一致性，切换后读新卷）。
//!
//! 范围：进程内（单副本）。多副本需共享门禁态（与 TTL 判据同批的生产
//! 前置，README §10 已披露多副本限制）。
//!
//! 在途写（复审 P1）：门禁只拦"门禁开启后到达"的写——开启前已通过门禁、
//! 正在异步执行的写（长上传/慢盘）不受 `is_migrating` 影响，若 cutover 的
//! 最终同步不等它们排空，落盘在旧卷 → 源/目标分叉。写准入在门禁通过后
//! 登记（每用户计数），cutover begin 后先排空再停 agent。

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use axum::http::Method;

static MIGRATING_USERS: LazyLock<std::sync::RwLock<HashSet<String>>> =
    LazyLock::new(|| std::sync::RwLock::new(HashSet::new()));

#[derive(Default)]
struct InflightWrites {
    counts: std::sync::Mutex<std::collections::HashMap<String, Arc<std::sync::atomic::AtomicUsize>>>,
    drained: tokio::sync::Notify,
}

static INFLIGHT_WRITES: LazyLock<InflightWrites> = LazyLock::new(InflightWrites::default);

/// 在途写登记 guard：Drop 时计数 -1 并唤醒排空等待。
pub struct WriteAdmissionGuard {
    user_id: String,
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for WriteAdmissionGuard {
    fn drop(&mut self) {
        if self.counter.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1 {
            INFLIGHT_WRITES.drained.notify_waiters();
        }
    }
}

/// 登记一笔已通过门禁的在途写（intercept 在写请求门禁通过后调用）。
pub fn write_admitted(user_id: &str) -> WriteAdmissionGuard {
    let counter = {
        let mut counts = INFLIGHT_WRITES
            .counts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        counts
            .entry(user_id.to_string())
            .or_insert_with(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)))
            .clone()
    };
    counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    WriteAdmissionGuard {
        user_id: user_id.to_string(),
        counter,
    }
}

/// 等待该用户在途写归零（cutover begin 后、停 agent 前调用）。
/// 返回超时后仍在途的笔数（0=干净排空；非 0 由调用方决策——继续切换
/// 接受该笔丢失风险，或中止）。通知-轮询混合：Notify 唤醒 + 50ms 轮询
/// 兜底（防止 notify 与登记间的竞态窗口漏唤醒）。
pub async fn drain_user_writes(user_id: &str, timeout: std::time::Duration) -> usize {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = {
            let counts = INFLIGHT_WRITES
                .counts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            counts
                .get(user_id)
                .map(|c| c.load(std::sync::atomic::Ordering::Acquire))
                .unwrap_or(0)
        };
        if remaining == 0 || tokio::time::Instant::now() >= deadline {
            return remaining;
        }
        let wait = tokio::time::sleep_until(std::cmp::min(
            deadline,
            tokio::time::Instant::now() + std::time::Duration::from_millis(50),
        ));
        tokio::select! {
            _ = INFLIGHT_WRITES.drained.notified() => {}
            _ = wait => {}
        }
    }
}

/// 标记用户迁移进行中（cutover 持锁后调用）。
pub fn begin(user_id: &str) {
    MIGRATING_USERS
        .write()
        .unwrap_or_else(|e| e.into_inner())  // 锁毒化恢复(项目禁 expect)
        .insert(user_id.to_string());
}

/// 清除标记（cutover 结束——成功/失败路径都必须调；orchestrate 用显式
/// 调用而非 Drop guard，确保失败信息打完后再放行）。
pub fn end(user_id: &str) {
    MIGRATING_USERS
        .write()
        .unwrap_or_else(|e| e.into_inner())  // 锁毒化恢复(项目禁 expect)
        .remove(user_id);
}

/// 该用户迁移是否进行中。
pub fn is_migrating(user_id: &str) -> bool {
    MIGRATING_USERS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains(user_id)
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
}
