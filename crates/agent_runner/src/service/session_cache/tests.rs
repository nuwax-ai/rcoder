use super::*;
use crate::model::SessionMessageType;
use chrono::Utc;

fn make_msg(sub_type: &str) -> UnifiedSessionMessage {
    UnifiedSessionMessage {
        session_id: "test-session".to_string(),
        message_type: SessionMessageType::AgentSessionUpdate,
        sub_type: sub_type.to_string(),
        data: serde_json::json!({}),
        timestamp: Utc::now(),
    }
}

/// 🔒 竞态修复锁定：并发 push 期间建立订阅，输出（replay + 实时）不得有重复 seq。
/// 修复前 create_new_connection「先注册 sender 后取 ring 快照」的窗口内，
/// worker 的 Push 会既写 ring（被快照捕获）又 try_send 给 sender（实时），
/// 同一 seq 双路到达（gRPC 流重复的根因）；修复后快照+注册在 worker 单线程
/// 内原子完成（SessionCommand::Subscribe）。
#[tokio::test]
async fn subscribe_never_duplicates_under_concurrent_push() {
    let sd = SessionData::new(4096).await;

    let pusher = {
        let sd = sd.clone();
        tokio::spawn(async move {
            for i in 0..2000u64 {
                sd.push_message(make_msg(&format!("m{i}")));
                if i % 100 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        })
    };

    for _round in 0..40 {
        let (conn_id, replay, mut rx, token) =
            sd.create_new_connection(64, 0).await.expect("subscribe");
        let mut seqs: Vec<u64> = replay.iter().map(|(s, _)| *s).collect();
        // 收一小段实时（50ms 窗口）
        let _ = tokio::time::timeout(std::time::Duration::from_millis(50), async {
            while let Some((s, _)) = rx.recv().await {
                seqs.push(s);
            }
        })
        .await;
        token.cancel();
        sd.close_connection(conn_id);

        let mut sorted = seqs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            seqs.len(),
            "duplicate seq in subscription output (round {_round}): {:?}",
            seqs.iter()
                .fold(Vec::new(), |mut acc: Vec<u64>, s| {
                    if acc.last() != Some(s) {
                        acc.push(*s);
                    }
                    acc
                })
                .len()
        );
    }
    pusher.await.expect("pusher");
}

#[tokio::test]
async fn replay_since_returns_only_messages_after_from_seq() {
    let sd = SessionData::new(64).await;
    sd.push_message(make_msg("a")); // seq 1
    sd.push_message(make_msg("b")); // seq 2
    sd.push_message(make_msg("c")); // seq 3

    let got: Vec<u64> = sd
        .replay_since(1)
        .await
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert_eq!(got, vec![2, 3], "replay_since(1) must return only seq>1");
}

#[tokio::test]
async fn prompt_error_flows_as_terminal_and_clears_ring() {
    // agent 异常退出链路锁定：notify_prompt_error 的 SessionPromptError 在
    // Notify→Message 转换层归一化为 SessionPromptEnd(sub_type="error")——
    // 终端即清必须覆盖异常路径（防回归：改转换映射会破坏此覆盖，测试会红）。
    let sid = "test-err-terminal";
    let sd = SessionData::new(64).await;
    SESSION_CACHE.insert(sid.to_string(), sd.clone());
    sd.push_message(make_msg("half_output_1")); // 异常前的半截输出
    sd.push_message(make_msg("half_output_2"));

    let notify = SessionNotify::SessionPromptError(shared_types::SessionPromptError {
        session_id: sid.to_string(),
        error: agent_client_protocol::Error::new(-32603, "agent exited abnormally"),
        request_id: None,
    });
    push_session_update_with_project("proj-err", sid, notify)
        .await
        .expect("push error notify");

    let got: Vec<u64> = sd
        .replay_since(0)
        .await
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert!(
        got.is_empty(),
        "ring must be empty after agent-abnormal-exit error notify, got {got:?}"
    );
}

#[tokio::test]
async fn terminal_event_clears_ring_immediately() {
    let sd = SessionData::new(64).await;
    sd.push_message(make_msg("a")); // seq 1
    sd.push_message(make_msg("b")); // seq 2
    let mut end = make_msg("end");
    end.message_type = SessionMessageType::SessionPromptEnd;
    sd.push_message(end); // 终端：推送后 ring 立即清空

    let got: Vec<u64> = sd
        .replay_since(0)
        .await
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert!(
        got.is_empty(),
        "ring must be empty right after terminal event, got {got:?}"
    );
}

#[tokio::test]
async fn seq_keeps_monotonic_across_clear() {
    let sd = SessionData::new(64).await;
    sd.push_message(make_msg("a")); // seq 1
    sd.push_message(make_msg("b")); // seq 2
    let cleared = sd.clear_message_buffer().await;
    assert_eq!(cleared, 2);
    sd.push_message(make_msg("c")); // seq 必须为 3（不随 clear 重置）

    let got: Vec<u64> = sd
        .replay_since(0)
        .await
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert_eq!(got, vec![3], "seq must remain monotonic after clear");
}

#[tokio::test]
async fn replay_since_is_non_destructive() {
    let sd = SessionData::new(64).await;
    sd.push_message(make_msg("a"));
    sd.push_message(make_msg("b"));

    let first: Vec<u64> = sd
        .replay_since(0)
        .await
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    let second: Vec<u64> = sd
        .replay_since(0)
        .await
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert_eq!(first, second);
    assert_eq!(first, vec![1, 2], "replay must not drain the buffer");
}

fn make_heartbeat() -> UnifiedSessionMessage {
    UnifiedSessionMessage {
        session_id: "test-session".to_string(),
        message_type: SessionMessageType::Heartbeat,
        sub_type: "ping".to_string(),
        data: serde_json::json!({}),
        timestamp: Utc::now(),
    }
}

#[tokio::test]
async fn heartbeat_not_buffered_and_does_not_advance_seq() {
    let sd = SessionData::new(64).await;
    sd.push_message(make_heartbeat()); // Heartbeat：不入 ring，seq=0，不递增 next_seq
    sd.push_message(make_msg("a")); // seq 1
    sd.push_message(make_heartbeat());
    sd.push_message(make_msg("b")); // seq 2（Heartbeat 不占 seq 号）

    let got: Vec<u64> = sd
        .replay_since(0)
        .await
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert_eq!(
        got,
        vec![1, 2],
        "Heartbeat must not be buffered; seq must skip it"
    );
}

#[tokio::test]
async fn ring_overflow_drops_oldest_and_keeps_seq_contiguous() {
    let sd = SessionData::new(3).await; // 容量 3
    sd.push_message(make_msg("a")); // seq 1
    sd.push_message(make_msg("b")); // seq 2
    sd.push_message(make_msg("c")); // seq 3
    sd.push_message(make_msg("d")); // seq 4，挤掉 seq1
    sd.push_message(make_msg("e")); // seq 5，挤掉 seq2

    let got: Vec<u64> = sd
        .replay_since(0)
        .await
        .into_iter()
        .map(|(s, _)| s)
        .collect();
    assert_eq!(
        got,
        vec![3, 4, 5],
        "ring overflow drops oldest, seq stays contiguous"
    );
}

/// P2-M2：多订阅者并存——新订阅不再 cancel 旧流（多端同看/多副本共享流的根修）；
/// close_connection 只关指定订阅者；close_all_connections 关全部。
#[tokio::test]
async fn multi_subscriber_coexistence_and_selective_close() {
    let session = SessionData::new(8).await;
    let (first_id, _first_replay, mut first_rx, first_cancel) = session
        .create_new_connection(8, 0)
        .await
        .expect("first connection");
    let (_second_id, _second_replay, mut second_rx, second_cancel) = session
        .create_new_connection(8, 0)
        .await
        .expect("second connection");

    // 旧语义：新订阅 cancel 旧流。新语义：两者并存
    assert!(
        !first_cancel.is_cancelled(),
        "new subscriber must NOT cancel existing stream"
    );
    assert!(!second_cancel.is_cancelled());

    // 两个订阅者都收到实时消息
    session.push_message(make_msg("m1"));
    assert_eq!(
        first_rx.recv().await.expect("first receives").1.sub_type,
        "m1"
    );
    assert_eq!(
        second_rx.recv().await.expect("second receives").1.sub_type,
        "m1"
    );

    // 关闭第一个：只影响自己
    session.close_connection(first_id);
    assert!(first_cancel.is_cancelled());
    assert!(
        first_rx.recv().await.is_none(),
        "closed subscriber sender must be dropped"
    );
    assert!(!second_cancel.is_cancelled(), "peer subscriber unaffected");
    session.push_message(make_msg("m2"));
    assert_eq!(
        second_rx
            .recv()
            .await
            .expect("second still live")
            .1
            .sub_type,
        "m2"
    );

    // close_all：清场语义（任务取消/会话停止）
    session.close_all_connections();
    assert!(second_cancel.is_cancelled());
    assert!(second_rx.recv().await.is_none());
}

/// P2-M2：订阅者上限——超过 MAX_SUBSCRIBERS 逐最旧
#[tokio::test]
async fn subscriber_limit_evicts_oldest() {
    let session = SessionData::new(8).await;
    let mut first_cancel = None;
    for i in 0..=(MAX_SUBSCRIBERS as u64) {
        let (_id, _replay, _rx, cancel) = session
            .create_new_connection(8, 0)
            .await
            .expect("connection {i}");
        if i == 0 {
            first_cancel = Some(cancel);
        }
    }
    assert!(
        first_cancel.expect("saved").is_cancelled(),
        "oldest subscriber evicted at limit"
    );
    // 数量封顶
    assert_eq!(session.connections_len(), MAX_SUBSCRIBERS);
}

fn make_agent_info(project_id: &str, session_id: &str) -> shared_types::ProjectAndAgentInfo {
    use std::sync::Arc;
    use tokio::sync::mpsc;
    shared_types::ProjectAndAgentInfo {
        project_id: project_id.to_string(),
        session_id: agent_client_protocol::schema::v1::SessionId::new(Arc::from(session_id)),
        prompt_tx: mpsc::channel(shared_types::AGENT_PROMPT_CHANNEL_CAPACITY).0,
        cancel_tx: mpsc::channel(shared_types::AGENT_CANCEL_CHANNEL_CAPACITY).0,
        model_provider: None,
        request_id: None,
        status: shared_types::AgentStatus::Idle,
        last_activity: Utc::now(),
        created_at: Utc::now(),
        stop_handle: None,
        agent_binary_snapshot: None,
    }
}

#[tokio::test]
async fn ensure_project_session_ignores_stale_session_id() {
    // C-slim 回归保护：agent 用过期/陌生 sessionId 推消息时，
    // 不得反向改写 project 映射、不得 cancel 当前正在工作的真实 SSE。
    let project = "cslim_stale_proj";
    let real_sid = "ses_real_active";
    let stale_sid = "753cf1fd-stale-not-registered";

    let registry = &AGENT_REGISTRY;
    registry.remove_by_project(project); // 幂等清理残留
    registry.register(project, real_sid, make_agent_info(project, real_sid));

    // stale_sid 从未注册 → get_project_by_session(stale_sid)=None → 只 buffer，返回 0
    let cleared = ensure_project_session(project, stale_sid).await;
    assert_eq!(
        cleared, 0,
        "stale sid must be ignored (buffer-only, no migration)"
    );
    assert_eq!(
        registry.get_session_by_project(project).as_deref(),
        Some(real_sid),
        "active session mapping must NOT be overwritten by a stale sid"
    );

    registry.remove_by_project(project);
}
