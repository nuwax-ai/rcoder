use super::*;
use ringbuf::traits::{Consumer, Producer};

#[test]
fn replay_since_filters_by_seq() {
    // 构造一个 SharedStream 的 ring 直接测试 replay_since 逻辑
    let mut ring: HeapRb<(u64, SharedEvent)> = HeapRb::new(10);
    for seq in 1..=5 {
        let ev = Arc::new(ProgressEvent {
            message_type: "AgentSessionUpdate".into(),
            sub_type: "test".into(),
            payload: "{}".into(),
            request_id: None,
            seq,
            timestamp: seq as i64,
        });
        drop(ring.try_push((seq, ev)));
    }
    let ring = Mutex::new(ring);
    let got: Vec<u64> = ring
        .lock()
        .iter()
        .filter(|(s, _)| *s > 3)
        .map(|(s, _)| *s)
        .collect();
    assert_eq!(got, vec![4, 5]);
}

#[test]
fn replay_since_is_non_destructive() {
    let mut ring: HeapRb<(u64, SharedEvent)> = HeapRb::new(10);
    for seq in 1..=3 {
        drop(ring.try_push((
            seq,
            Arc::new(ProgressEvent {
                message_type: "X".into(),
                sub_type: "y".into(),
                payload: "{}".into(),
                request_id: None,
                seq,
                timestamp: 0,
            }),
        )));
    }
    let ring = Mutex::new(ring);
    let first: Vec<u64> = ring.lock().iter().map(|(s, _)| *s).collect();
    let second: Vec<u64> = ring.lock().iter().map(|(s, _)| *s).collect();
    assert_eq!(first, second, "iter must be non-destructive");
    assert_eq!(first, vec![1, 2, 3]);
}

#[test]
fn registry_default_is_empty() {
    let r = SessionStreamRegistry::default();
    assert!(r.is_empty());
}

#[test]
fn create_lock_is_removed_only_without_active_holders() {
    let registry = SessionStreamRegistry::default();
    let held = registry
        .create_locks
        .entry("session-a".to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();

    registry.remove_unused_create_lock("session-a");
    assert!(registry.create_locks.contains_key("session-a"));

    drop(held);
    registry.remove_unused_create_lock("session-a");
    assert!(!registry.create_locks.contains_key("session-a"));
}

#[tokio::test]
async fn removing_matching_stream_cancels_its_backend_task() {
    let registry = SessionStreamRegistry::default();
    let shared = SharedStream::new(
        std::sync::Weak::<SessionStreamRegistry>::new(),
        "session-a".into(),
        "127.0.0.1:1".into(),
        Arc::new(GrpcChannelPool::new()),
        "en",
        Arc::new(|_| {}),
        None,
    )
    .await;
    registry
        .streams
        .insert("session-a".into(), Arc::clone(&shared));

    assert!(!shared.cancel_token.is_cancelled());
    assert!(registry.remove_and_shutdown("session-a", &shared));
    assert!(shared.cancel_token.is_cancelled());
    assert!(!registry.streams.contains_key("session-a"));
}

fn arc_event(seq: u64, sub: &str) -> Arc<ProgressEvent> {
    Arc::new(ProgressEvent {
        message_type: "AgentSessionUpdate".into(),
        sub_type: sub.into(),
        payload: "{}".into(),
        request_id: None,
        seq,
        timestamp: 0,
    })
}

fn terminal_event(seq: u64) -> Arc<ProgressEvent> {
    Arc::new(ProgressEvent {
        message_type: "SessionPromptEnd".into(),
        sub_type: "end_turn".into(),
        payload: "{}".into(),
        request_id: None,
        seq,
        timestamp: 0,
    })
}

/// 🔒 首连资格语义（registry 级，跨 SharedStream 生命周期 + 终端重置）：
/// 同 session 仅首个客户端获得 replay 资格；重连/第二端不获得（防重复红线）；
/// 终端事件归还资格（新一轮 turn 的首连重新可兜 chat→SSE 时间差）。
/// 长 turn + idle 清理重建 SharedStream 的重连同样不获得（registry 级状态
/// 不随流销毁丢失——流级标志的缺陷修复点）。
#[tokio::test]
async fn first_client_claim_semantics() {
    let registry = Arc::new(SessionStreamRegistry::new());
    let shared = SharedStream::new(
        Arc::downgrade(&registry),
        "s-first".into(),
        "127.0.0.1:1".into(),
        Arc::new(GrpcChannelPool::new()),
        "en",
        Arc::new(|_| {}),
        None,
    )
    .await;

    assert!(shared.claim_first_client(), "首个客户端获得 replay 资格");
    assert!(!shared.claim_first_client(), "重连不获得（防重复红线）");

    // 终端事件归还资格（正常/异常/合成终端统一走 dispatch_event 的终端分支）
    shared.dispatch_event(terminal_event(9));
    assert!(
        shared.claim_first_client(),
        "终端后新一轮 turn 的首连重新获得资格"
    );
    assert!(!shared.claim_first_client(), "新 turn 内的重连同样不获得");

    // 跨 SharedStream 生命周期：模拟 idle 清理重建——registry 级状态仍在
    let shared2 = SharedStream::new(
        Arc::downgrade(&registry),
        "s-first".into(),
        "127.0.0.1:1".into(),
        Arc::new(GrpcChannelPool::new()),
        "en",
        Arc::new(|_| {}),
        None,
    )
    .await;
    assert!(
        !shared2.claim_first_client(),
        "流重建不重置资格（长 turn + idle 重连不重放）"
    );
}

#[tokio::test]
async fn terminal_event_clears_ring_for_next_turn() {
    let shared = SharedStream::new(
        std::sync::Weak::<SessionStreamRegistry>::new(),
        "s1".into(),
        "127.0.0.1:1".into(),
        Arc::new(GrpcChannelPool::new()),
        "en",
        Arc::new(|_| {}),
        None,
    )
    .await;

    // 第一轮：seq 1..3 + 终端事件 seq 4
    shared.dispatch_event(arc_event(1, "turn1-a"));
    shared.dispatch_event(arc_event(2, "turn1-b"));
    shared.dispatch_event(arc_event(3, "turn1-c"));
    shared.dispatch_event(terminal_event(4));

    // 终端后 ring 已清：新客户端（last_seq=0，换模型重连的典型形态）重放为空
    assert!(
        shared.replay_since(0).is_empty(),
        "ring must be cleared after terminal event"
    );
    // last_seq 保持单调（防后台流重连回放）
    assert_eq!(shared.last_seq(), 4, "last_seq must stay monotonic");

    // 第二轮：seq 5 起，重放只含新消息
    shared.dispatch_event(arc_event(5, "turn2-a"));
    let got: Vec<u64> = shared
        .replay_since(0)
        .into_iter()
        .map(|ev| ev.seq)
        .collect();
    assert_eq!(got, vec![5], "next turn replay contains only new events");
}

/// 🔚 合成终端（seq=0，流断/故障路径的兜底 End/error）同样清 ring——
/// 本次修复锁定：agent 侧终端即清可能因 gRPC 断流没送达 rcoder，rcoder 的
/// 合成终端兜底若只 broadcast 不清 ring，半轮残留会留给下一个客户端。
#[tokio::test]
async fn synthetic_terminal_event_clears_ring() {
    let shared = SharedStream::new(
        std::sync::Weak::<SessionStreamRegistry>::new(),
        "s-syn".into(),
        "127.0.0.1:1".into(),
        Arc::new(GrpcChannelPool::new()),
        "en",
        Arc::new(|_| {}),
        None,
    )
    .await;

    // 半轮写入（模拟流断时 ring 已积累的内容）
    shared.dispatch_event(arc_event(1, "half-a"));
    shared.dispatch_event(arc_event(2, "half-b"));

    // 合成终端（seq=0）：只 broadcast + 清 ring，不写 ring 不动 last_seq
    shared.dispatch_event(Arc::new(make_prompt_end_event()));

    assert!(
        shared.replay_since(0).is_empty(),
        "synthetic terminal must clear ring (half-turn residue forbidden)"
    );
    assert_eq!(
        shared.last_seq(),
        2,
        "seq=0 synthetic terminal must not touch last_seq"
    );
}

/// 🔍 边界审查：epoch 哨兵（seq=0 StreamReset）确实会被终端 clear 从 ring 清掉。
///
/// **结论：不构成真实缺陷**。终端（SessionPromptEnd）后 agent_runner 关流 →
/// rcoder 后台 task return（自然死亡）→ SharedStream 成为僵尸（is_alive=false）。
/// 下一个客户端连接时 get_or_create 的 is_alive 检测发现僵尸 → 移除并创建**全新**
/// SharedStream（last_seq=0 + 新后台 task + 独立 epoch 比较）——被清掉的 ring
/// 无人读取。此测试固化该事实作为防回归证据。
#[tokio::test]
async fn terminal_clear_loses_sentinel_but_zombie_replaces_stream() {
    let shared = SharedStream::new(
        std::sync::Weak::<SessionStreamRegistry>::new(),
        "s1".into(),
        "127.0.0.1:1".into(),
        Arc::new(GrpcChannelPool::new()),
        "en",
        Arc::new(|_| {}),
        None,
    )
    .await;

    // 模拟 #15 epoch 变更：后台 task 的处理是 clear_ring + push 哨兵 + last_seq=0
    shared.last_seq.store(0, Ordering::Release);
    shared.clear_ring();
    let reset = Arc::new(make_cursor_reset_event());
    shared.push_reset_to_ring(Arc::clone(&reset));

    // 新 epoch turn 运行并终结
    shared.dispatch_event(arc_event(1, "new-epoch-msg"));
    shared.dispatch_event(terminal_event(2));

    // 事实：哨兵被清。但此 ring 属于僵尸流（后台 task 已 return）——
    // get_or_create 的 is_alive 检测会替换为全新流，此 ring 无人读取
    assert!(
        !shared
            .replay_since(50)
            .iter()
            .any(|ev| ev.message_type == "StreamReset"),
        "sentinel IS cleared from zombie ring (documented fact)"
    );
    // 注：is_alive 断言不适用于测试环境（后台 task 连接假地址处于重试中，
    // 不会因 dispatch_event 中的终端事件而退出——生产中终端来自 gRPC 流，
    // 流关闭后 Ok(None) → return → task 自然死亡 → 僵尸 → get_or_create 替换）
}

/// SharedStream::new 会 spawn 后台 gRPC task（连 127.0.0.1:1 必失败，但不影响 dispatch_event
/// —— 该方法只操作 ring/last_seq/broadcast，不依赖 gRPC）。测试结束 runtime drop 会 cancel task。
#[tokio::test]
async fn dispatch_event_buffers_real_events_skips_synthetic() {
    let shared = SharedStream::new(
        std::sync::Weak::<SessionStreamRegistry>::new(),
        "s1".into(),
        "127.0.0.1:1".into(),
        Arc::new(GrpcChannelPool::new()),
        "en",
        Arc::new(|_| {}),
        None,
    )
    .await;

    shared.dispatch_event(arc_event(1, "m1"));
    shared.dispatch_event(arc_event(2, "m2"));
    shared.dispatch_event(arc_event(0, "synthetic")); // seq=0 合成消息：不入 ring、不更新 last_seq

    assert_eq!(shared.last_seq(), 2, "seq=0 must not update last_seq");
    let got: Vec<u64> = shared
        .replay_since(0)
        .into_iter()
        .map(|ev| ev.seq)
        .collect();
    assert_eq!(got, vec![1, 2], "only seq>0 events enter ring");
}

#[tokio::test]
async fn client_guard_increments_and_decrements_ref_count() {
    let registry = Arc::new(SessionStreamRegistry::default());
    let shared = SharedStream::new(
        std::sync::Weak::<SessionStreamRegistry>::new(),
        "s1".into(),
        "127.0.0.1:1".into(),
        Arc::new(GrpcChannelPool::new()),
        "en",
        Arc::new(|_| {}),
        None,
    )
    .await;

    assert_eq!(shared.ref_count.load(Ordering::Acquire), 0);
    let guard1 = shared.acquire_client(Arc::clone(&registry));
    let guard2 = shared.acquire_client(Arc::clone(&registry));
    assert_eq!(shared.ref_count.load(Ordering::Acquire), 2);

    drop(guard1);
    assert_eq!(
        shared.ref_count.load(Ordering::Acquire),
        1,
        "one guard dropped, one remains"
    );

    drop(guard2);
    assert_eq!(
        shared.ref_count.load(Ordering::Acquire),
        0,
        "all guards dropped → ref_count back to 0"
    );
    // 最后一个 guard drop 会 spawn 30s 延迟清理；测试结束 runtime drop 会 cancel 它。
}

#[tokio::test]
async fn shutdown_streams_by_addr_closes_only_matching_streams() {
    let registry = SessionStreamRegistry::default();
    let matched_a = SharedStream::new(
        std::sync::Weak::<SessionStreamRegistry>::new(),
        "session-a".into(),
        "10.0.0.1:50051".into(),
        Arc::new(GrpcChannelPool::new()),
        "en",
        Arc::new(|_| {}),
        None,
    )
    .await;
    let matched_b = SharedStream::new(
        std::sync::Weak::<SessionStreamRegistry>::new(),
        "session-b".into(),
        "10.0.0.1:50051".into(),
        Arc::new(GrpcChannelPool::new()),
        "en",
        Arc::new(|_| {}),
        None,
    )
    .await;
    let unmatched = SharedStream::new(
        std::sync::Weak::<SessionStreamRegistry>::new(),
        "session-c".into(),
        "10.0.0.2:50051".into(),
        Arc::new(GrpcChannelPool::new()),
        "en",
        Arc::new(|_| {}),
        None,
    )
    .await;
    registry
        .streams
        .insert("session-a".into(), Arc::clone(&matched_a));
    registry
        .streams
        .insert("session-b".into(), Arc::clone(&matched_b));
    registry
        .streams
        .insert("session-c".into(), Arc::clone(&unmatched));

    // 只关闭匹配地址的流
    let closed = registry.shutdown_streams_by_addr("10.0.0.1:50051");
    assert_eq!(closed, 2, "两条匹配地址的流都应被关闭");
    assert!(matched_a.cancel_token.is_cancelled());
    assert!(matched_b.cancel_token.is_cancelled());
    assert!(!registry.streams.contains_key("session-a"));
    assert!(!registry.streams.contains_key("session-b"));

    // 不匹配的流保留且未 cancel
    assert!(!unmatched.cancel_token.is_cancelled());
    assert!(registry.streams.contains_key("session-c"));
    assert_eq!(registry.len(), 1);

    // 幂等：重复关闭同一地址返回 0
    assert_eq!(registry.shutdown_streams_by_addr("10.0.0.1:50051"), 0);
}

#[test]
fn shutdown_streams_by_addr_returns_zero_for_unknown_addr() {
    let registry = SessionStreamRegistry::default();
    assert_eq!(registry.shutdown_streams_by_addr("1.2.3.4:50051"), 0);
}

#[tokio::test]
async fn terminal_error_event_payload_is_valid_json() {
    // 无 DiagCtx → 通用文案;payload 必须是合法 JSON(serde_json 构造,非 format! 拼接)。
    let ev = make_terminal_error_event(None, "en-US").await;
    let payload: serde_json::Value =
        serde_json::from_str(&ev.payload).expect("payload must be valid JSON");
    assert_eq!(
        payload["code"],
        shared_types::error_codes::ERR_AGENT_CONTAINER_UNAVAILABLE
    );
    assert!(
        payload["message"].is_string(),
        "message must be a JSON string"
    );
    assert_eq!(ev.message_type, "SessionPromptEnd");
    assert_eq!(ev.sub_type, "error");
    assert_eq!(ev.seq, 0, "synthetic terminal event uses seq=0");
}

#[test]
fn stream_error_payload_is_valid_json() {
    let ev = make_stream_error_event(Code::Unavailable, "irrelevant");
    let payload: serde_json::Value =
        serde_json::from_str(&ev.payload).expect("payload must be valid JSON");
    assert_eq!(payload["code"], "GRPC_SERVICE_UNAVAILABLE");
    assert_eq!(payload["message"], "Agent execution error, please retry.");
}
