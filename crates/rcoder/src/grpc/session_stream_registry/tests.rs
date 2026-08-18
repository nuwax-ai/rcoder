//! per-client 转发架构下的注册表测试（去 ring 重构后）：
//! 首连资格语义、活跃流登记与按 addr/session 关闭、合成事件 payload 合法性。

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::*;

fn registry() -> Arc<SessionStreamRegistry> {
    Arc::new(SessionStreamRegistry::new())
}

#[test]
fn registry_default_is_empty() {
    let reg = SessionStreamRegistry::default();
    assert!(reg.is_empty());
    assert_eq!(reg.len(), 0);
}

/// 首连资格：首次 claim true（from_seq=0 兜时间差），后续 false（live-only
/// 不重放——防重复红线），turn 终态归还后新一轮首连重新 true。
#[tokio::test]
async fn first_client_claim_semantics() {
    let reg = registry();
    assert!(reg.claim_first_client("s1"), "first client claims");
    assert!(!reg.claim_first_client("s1"), "second client is live-only");
    assert!(!reg.claim_first_client("s1"), "reconnect stays live-only");
    // turn 终态归还 → 新一轮 turn 的首连重新获得资格
    reg.release_first_client_claim("s1");
    assert!(reg.claim_first_client("s1"), "claim restored after turn terminal");
    // cancelled 不归还（业务常态事件，调用方不触发 release——此处验证 release 本身幂等）
    reg.release_first_client_claim("s1");
    assert!(!reg.claim_first_client("s1"), "double release must not re-grant");
}

/// 跨 session 隔离：s1 的资格状态不影响 s2
#[tokio::test]
async fn claim_is_per_session() {
    let reg = registry();
    assert!(reg.claim_first_client("s1"));
    assert!(reg.claim_first_client("s2"), "independent sessions claim independently");
}

fn token_pair() -> (Arc<CancellationToken>, Arc<CancellationToken>) {
    (Arc::new(CancellationToken::new()), Arc::new(CancellationToken::new()))
}

/// 按 addr 关闭：只取消匹配地址的登记流，失效 Weak 一并回收
#[tokio::test]
async fn shutdown_streams_by_addr_cancels_only_matching() {
    let reg = registry();
    let (t1, t2) = token_pair();
    reg.register_stream("10.0.0.1:50051", "s1", &t1);
    reg.register_stream("10.0.0.2:50051", "s2", &t2);

    let closed = reg.shutdown_streams_by_addr("10.0.0.1:50051");
    assert_eq!(closed, 1);
    assert!(t1.is_cancelled(), "matching stream cancelled");
    assert!(!t2.is_cancelled(), "other addr untouched");
    // 幂等
    assert_eq!(reg.shutdown_streams_by_addr("10.0.0.1:50051"), 0);
}

/// 按 session 关闭：全表扫描匹配 session 的登记流
#[tokio::test]
async fn shutdown_session_cancels_matching_sessions() {
    let reg = registry();
    let (t1, t2) = token_pair();
    reg.register_stream("10.0.0.1:50051", "s1", &t1);
    reg.register_stream("10.0.0.1:50051", "s2", &t2);

    assert!(reg.shutdown_session("s1"));
    assert!(t1.is_cancelled());
    assert!(!t2.is_cancelled(), "other session untouched");
}

/// task 自然结束（token drop）后 Weak 失效：下次登记懒惰回收，不泄漏计数
#[tokio::test]
async fn dead_registrations_are_lazily_collected() {
    let reg = registry();
    let (t1, t2) = token_pair();
    reg.register_stream("addr", "s1", &t1);
    drop(t1);
    reg.register_stream("addr", "s2", &t2);
    assert_eq!(reg.len(), 1, "dead Weak entry collected on next register");
    // 关闭时失效条目计入 matched 但无法 cancel（已死），活条目正常取消
    let closed = reg.shutdown_streams_by_addr("addr");
    assert_eq!(closed, 1);
    assert!(t2.is_cancelled());
}

#[tokio::test]
async fn shutdown_streams_by_addr_returns_zero_for_unknown_addr() {
    let reg = registry();
    assert_eq!(reg.shutdown_streams_by_addr("unknown:1"), 0);
}

/// 终态错误事件 payload 必须是合法 JSON（前端解析依赖）
#[tokio::test]
async fn terminal_error_event_payload_is_valid_json() {
    let ev = make_terminal_error_event(None, "en").await;
    assert_eq!(ev.message_type, "SessionPromptEnd");
    assert_eq!(ev.sub_type, "error");
    assert!(serde_json::from_str::<serde_json::Value>(&ev.payload).is_ok());
}

#[test]
fn stream_error_payload_is_valid_json() {
    let ev = make_stream_error_event(tonic::Code::Unavailable, "boom");
    assert_eq!(ev.sub_type, "error");
    assert!(serde_json::from_str::<serde_json::Value>(&ev.payload).is_ok());
}

/// cursor-reset 哨兵：非终态（不关流），seq=0（不污染游标）
#[test]
fn cursor_reset_event_is_non_terminal_zero_seq() {
    let ev = make_cursor_reset_event();
    assert_eq!(ev.message_type, "StreamReset");
    assert_eq!(ev.seq, 0);
    assert!(!is_stream_closing(&ev.message_type, &ev.sub_type));
    assert!(!is_turn_terminal(&ev.message_type, &ev.sub_type));
}

/// 终端判定分档：end_turn/error 是 turn 边界 + 关流；stream_ended 仅关流；
/// cancelled 两档都不触发（用户连发消息自动取消的常态事件）
#[test]
fn terminal_classification_semantics() {
    assert!(is_turn_terminal("SessionPromptEnd", "end_turn"));
    assert!(is_turn_terminal("SessionPromptEnd", "error"));
    assert!(!is_turn_terminal("SessionPromptEnd", "cancelled"));
    assert!(is_stream_closing("SessionPromptEnd", "stream_ended"));
    assert!(!is_stream_closing("SessionPromptEnd", "cancelled"));
    assert!(!is_stream_closing("AgentSessionUpdate", "agent_message_chunk"));
}
