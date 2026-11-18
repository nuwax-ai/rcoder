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

use std::collections::HashSet;
use std::sync::LazyLock;

use axum::http::Method;

static MIGRATING_USERS: LazyLock<std::sync::RwLock<HashSet<String>>> =
    LazyLock::new(|| std::sync::RwLock::new(HashSet::new()));

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
