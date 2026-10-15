//! 存储契约断言全集：进程内实现与 PG 实现必须双双通过（语义不漂移的护栏）。
//!
//! 覆盖：受理前置（Ready 幂等 / Starting 阻塞 / Unknown 证据门禁）、CAS（旧
//! revision/operation/instance 写回失败、迟到失败不覆盖新实例）、端口全局唯一
//! （占用集合联动、指定端口冲突拒绝）、心跳/activity 语义、启动对账范围。
//! 本模块为公开测试支撑（非 `#[cfg(test)]`）：rcoder-storage 的 PG 契约测试
//! 以 dev-dependency 引入本函数复跑。
use shared_types::{
    AcceptStartInput, AcceptStartOutcome, PreviewHostIdentity, PreviewInstanceState,
    PreviewLifecycleStore, PreviewStoreError,
};

fn host(uid: &str, boot: &str) -> PreviewHostIdentity {
    PreviewHostIdentity {
        host_id: format!("{uid}:{boot}"),
        pod_name: Some(format!("pod-{uid}")),
        pod_ip: Some("10.0.0.1".into()),
    }
}

fn start_input(key: &str, project: &str, op: &str, instance: &str) -> AcceptStartInput {
    AcceptStartInput {
        preview_key: key.to_string(),
        project_id: project.to_string(),
        project_path: format!("/ws/{project}"),
        host: host("uid-a", "boot-1"),
        operation_id: op.to_string(),
        instance_id: instance.to_string(),
        requested_port: None,
        recover_unknown_evidence: None,
    }
}

fn assert_conflict<T>(result: &Result<T, PreviewStoreError>) {
    assert!(
        matches!(result, Err(PreviewStoreError::Conflict(_))),
        "expected Conflict, got {:?}",
        result.as_ref().err()
    );
}

/// 契约全集。store 须为空库（测试自行清理或用独立数据库）。
pub async fn run(store: &dyn PreviewLifecycleStore) {
    admitted_and_publish(store).await;
    idempotent_ready_start(store).await;
    blocked_while_starting(store).await;
    stop_flow_and_reaccept(store).await;
    late_writer_cannot_overwrite_new_instance(store).await;
    unknown_requires_evidence(store).await;
    port_allocation_uniqueness(store).await;
    activity_and_heartbeat(store).await;
    host_reboot_reconciliation(store).await;
    failure_then_reaccept(store).await;
    lookup_views(store).await;
    generation_bound_recovery_and_stop(store).await;
}

async fn admitted_and_publish(store: &dyn PreviewLifecycleStore) {
    let outcome = store
        .accept_start(start_input("k1", "p1", "op-1", "inst-1"))
        .await
        .expect("fresh accept");
    let AcceptStartOutcome::Admitted(record) = outcome else {
        panic!("fresh accept must be Admitted");
    };
    assert_eq!(record.state, PreviewInstanceState::Starting);
    assert_eq!(record.revision, 1);
    let port = record.port.expect("port allocated on admission");
    assert!(shared_types::is_preview_port(port));

    // 错误 revision 的发布必须失败（CAS）。
    let bad = store
        .publish_running("k1", "op-1", record.revision + 1, 11, port, None)
        .await;
    assert_conflict(&bad);
    // 错误 operation 的发布必须失败。
    let bad = store
        .publish_running("k1", "op-other", record.revision, 11, port, None)
        .await;
    assert_conflict(&bad);

    let ready = store
        .publish_running("k1", "op-1", record.revision, 4242, port, Some("/page/"))
        .await
        .expect("publish");
    assert_eq!(ready.state, PreviewInstanceState::Ready);
    assert_eq!(ready.pid, Some(4242));
    assert_eq!(ready.port, Some(port));
    assert_eq!(ready.base_path.as_deref(), Some("/page/"));
    assert!(ready.last_heartbeat_at.is_some(), "publish 刷心跳");

    // 清理：走到 stopped，供后续用例独立。
    let stopping = store.accept_stop("k1", "op-stop-1").await.expect("stop");
    assert_eq!(stopping.state, PreviewInstanceState::Stopping);
    assert_eq!(stopping.revision, ready.revision + 1);
    let stopped = store
        .mark_stopped("k1", "op-stop-1", stopping.revision)
        .await
        .expect("mark stopped");
    assert_eq!(stopped.state, PreviewInstanceState::Stopped);
}

async fn idempotent_ready_start(store: &dyn PreviewLifecycleStore) {
    let admitted = store
        .accept_start(start_input("k2", "p2", "op-2", "inst-2"))
        .await
        .expect("accept");
    let AcceptStartOutcome::Admitted(record) = admitted else {
        panic!("must admit");
    };
    store
        .publish_running(
            "k2",
            "op-2",
            record.revision,
            100,
            record.port.unwrap(),
            None,
        )
        .await
        .expect("publish");

    let again = store
        .accept_start(start_input("k2", "p2", "op-2b", "inst-2b"))
        .await
        .expect("second start on ready");
    match again {
        AcceptStartOutcome::ExistingReady(current) => {
            assert_eq!(
                current.instance_id, "inst-2",
                "幂等返回当前实例，不开新实例"
            );
        }
        other => panic!("ready 上的重复 start 必须幂等，got {other:?}"),
    }

    store.accept_stop("k2", "op-stop-2").await.expect("stop");
    let row = store.get("k2").await.expect("get").expect("row");
    store
        .mark_stopped("k2", "op-stop-2", row.revision)
        .await
        .expect("stopped");
}

async fn blocked_while_starting(store: &dyn PreviewLifecycleStore) {
    let admitted = store
        .accept_start(start_input("k3", "p3", "op-3", "inst-3"))
        .await
        .expect("accept");
    assert!(matches!(admitted, AcceptStartOutcome::Admitted(_)));

    let blocked = store
        .accept_start(start_input("k3", "p3", "op-3b", "inst-3b"))
        .await
        .expect("second start while starting");
    assert!(
        matches!(blocked, AcceptStartOutcome::Blocked(_)),
        "starting 中的重复受理必须阻塞"
    );

    // 清理：直接标失败（CAS on starting）。
    let row = store.get("k3").await.unwrap().unwrap();
    store
        .mark_failed("k3", &row.instance_id, row.revision, "test cleanup")
        .await
        .expect("mark failed");
}

async fn stop_flow_and_reaccept(store: &dyn PreviewLifecycleStore) {
    let admitted = store
        .accept_start(start_input("k4", "p4", "op-4", "inst-4"))
        .await
        .expect("accept");
    let AcceptStartOutcome::Admitted(record) = admitted else {
        panic!("must admit");
    };
    store
        .publish_running(
            "k4",
            "op-4",
            record.revision,
            200,
            record.port.unwrap(),
            None,
        )
        .await
        .expect("publish");

    // 无实例键的 stop 拒绝；Ready 键受理。
    let missing = store.accept_stop("k-none", "op-x").await;
    assert!(matches!(missing, Err(PreviewStoreError::Invalid(_))));

    let stopping = store.accept_stop("k4", "op-stop-4").await.expect("stop");
    // 错误 revision 的 mark_stopped 失败。
    let bad = store
        .mark_stopped("k4", "op-stop-4", stopping.revision + 5)
        .await;
    assert_conflict(&bad);
    store
        .mark_stopped("k4", "op-stop-4", stopping.revision)
        .await
        .expect("stopped");

    // stopped 后再 stop：幂等返回 stopped。
    let again = store
        .accept_stop("k4", "op-stop-4b")
        .await
        .expect("idem stop");
    assert_eq!(again.state, PreviewInstanceState::Stopped);

    // stopped 后可重新受理：新 instance_id、revision 递增、新端口。
    let reaccept = store
        .accept_start(start_input("k4", "p4", "op-4b", "inst-4b"))
        .await
        .expect("reaccept after stop");
    let AcceptStartOutcome::Admitted(new_record) = reaccept else {
        panic!("must admit after stopped");
    };
    assert_eq!(new_record.instance_id, "inst-4b");
    assert_eq!(new_record.revision, stopping.revision + 1);
    let row = store.get("k4").await.unwrap().unwrap();
    store
        .mark_failed("k4", &row.instance_id, row.revision, "test cleanup")
        .await
        .expect("cleanup");
}

async fn late_writer_cannot_overwrite_new_instance(store: &dyn PreviewLifecycleStore) {
    // 场景：旧实例 v1 停止 → 新实例 v2 受理发布后，v1 的迟到失败/心跳/停止
    // 写回不得覆盖 v2。
    let v1 = store
        .accept_start(start_input("k5", "p5", "op-5", "inst-5"))
        .await
        .expect("v1 accept");
    let AcceptStartOutcome::Admitted(v1) = v1 else {
        panic!("must admit")
    };
    store
        .publish_running("k5", "op-5", v1.revision, 300, v1.port.unwrap(), None)
        .await
        .expect("v1 publish");
    let stopping = store.accept_stop("k5", "op-stop-5").await.expect("stop");
    store
        .mark_stopped("k5", "op-stop-5", stopping.revision)
        .await
        .expect("stopped");

    let v2 = store
        .accept_start(start_input("k5", "p5", "op-5b", "inst-5b"))
        .await
        .expect("v2 accept");
    let AcceptStartOutcome::Admitted(v2) = v2 else {
        panic!("must admit")
    };
    store
        .publish_running("k5", "op-5b", v2.revision, 301, v2.port.unwrap(), None)
        .await
        .expect("v2 publish");

    // 旧实例迟到失败：instance_id 不匹配 → Conflict，v2 保持 Ready。
    let late = store
        .mark_failed("k5", "inst-5", v1.revision, "late failure")
        .await;
    assert_conflict(&late);
    // 旧实例迟到心跳：返回 None。
    let late_hb = store.refresh_heartbeat("k5", "inst-5").await.expect("hb");
    assert!(late_hb.is_none());
    // 当前 = v2 Ready。
    let current = store.get("k5").await.unwrap().unwrap();
    assert_eq!(current.state, PreviewInstanceState::Ready);
    assert_eq!(current.instance_id, "inst-5b");

    store.accept_stop("k5", "op-stop-5b").await.expect("stop");
    let row = store.get("k5").await.unwrap().unwrap();
    store
        .mark_stopped("k5", "op-stop-5b", row.revision)
        .await
        .expect("cleanup");
}

async fn unknown_requires_evidence(store: &dyn PreviewLifecycleStore) {
    let v1 = store
        .accept_start(start_input("k6", "p6", "op-6", "inst-6"))
        .await
        .expect("accept");
    let AcceptStartOutcome::Admitted(v1) = v1 else {
        panic!("must admit")
    };
    store
        .publish_running("k6", "op-6", v1.revision, 400, v1.port.unwrap(), None)
        .await
        .expect("publish");

    // 转 Unknown 后：无证据的 start 被阻塞。
    store
        .mark_unknown("k6", "inst-6", "registry lost")
        .await
        .expect("mark unknown");
    let blocked = store
        .accept_start(start_input("k6", "p6", "op-6b", "inst-6b"))
        .await
        .expect("start on unknown");
    assert!(
        matches!(blocked, AcceptStartOutcome::Blocked(_)),
        "unknown 无证据必须阻塞"
    );

    // 证据落地：resolve → stopped → 可受理新实例；或带证据直接受理（同事务两步）。
    store
        .resolve_unknown_stopped("k6", "inst-6", "pod gone (kube verified)")
        .await
        .expect("resolve");
    let reaccept = store
        .accept_start(start_input("k6", "p6", "op-6c", "inst-6c"))
        .await
        .expect("accept after evidence");
    assert!(matches!(reaccept, AcceptStartOutcome::Admitted(_)));

    // 带 recover 证据的受理路径（不再先 resolve）。
    store
        .mark_unknown("k6", "inst-6c", "again lost")
        .await
        .expect("unknown again");
    let mut with_evidence = start_input("k6", "p6", "op-6d", "inst-6d");
    let current = store.get("k6").await.unwrap().unwrap();
    with_evidence.recover_unknown_evidence = Some(shared_types::PreviewRecoveryEvidence {
        instance_id: current.instance_id,
        revision: current.revision,
        detail: "pod gone (kube verified)".into(),
    });
    let admitted = store
        .accept_start(with_evidence)
        .await
        .expect("accept with evidence");
    let AcceptStartOutcome::Admitted(record) = admitted else {
        panic!("must admit with evidence");
    };
    assert!(record.detail.is_some(), "证据落 detail");

    store.accept_stop("k6", "op-stop-6").await.expect("stop");
    let row = store.get("k6").await.unwrap().unwrap();
    store
        .mark_stopped("k6", "op-stop-6", row.revision)
        .await
        .expect("cleanup");
}

async fn port_allocation_uniqueness(store: &dyn PreviewLifecycleStore) {
    let a = store
        .accept_start(start_input("k7a", "p7a", "op-7a", "inst-7a"))
        .await
        .expect("accept a");
    let b = store
        .accept_start(start_input("k7b", "p7b", "op-7b", "inst-7b"))
        .await
        .expect("accept b");
    let (AcceptStartOutcome::Admitted(a), AcceptStartOutcome::Admitted(b)) = (a, b) else {
        panic!("both must admit");
    };
    assert_ne!(a.port, b.port, "两个活跃实例不得同端口");

    // 指定已占用端口 → Invalid；池外端口 → Invalid。
    let mut occupied_req = start_input("k7c", "p7c", "op-7c", "inst-7c");
    occupied_req.requested_port = a.port;
    let err = store.accept_start(occupied_req).await;
    assert!(matches!(err, Err(PreviewStoreError::Invalid(_))));

    let mut out_pool = start_input("k7c", "p7c", "op-7c", "inst-7c");
    out_pool.requested_port = Some(8086);
    let err = store.accept_start(out_pool).await;
    assert!(matches!(err, Err(PreviewStoreError::Invalid(_))));

    // 指定空闲端口可用。
    let mut free_req = start_input("k7c", "p7c", "op-7c", "inst-7c");
    free_req.requested_port = Some(4901);
    let admitted = store.accept_start(free_req).await.expect("free port");
    assert!(matches!(admitted, AcceptStartOutcome::Admitted(_)));

    for row in store.list_active().await.expect("list") {
        store
            .mark_failed(
                &row.preview_key,
                &row.instance_id,
                row.revision,
                "test cleanup",
            )
            .await
            .expect("cleanup");
    }
}

async fn activity_and_heartbeat(store: &dyn PreviewLifecycleStore) {
    let admitted = store
        .accept_start(start_input("k8", "p8", "op-8", "inst-8"))
        .await
        .expect("accept");
    let AcceptStartOutcome::Admitted(record) = admitted else {
        panic!("must admit")
    };
    let port = record.port.unwrap();
    store
        .publish_running("k8", "op-8", record.revision, 500, port, None)
        .await
        .expect("publish");

    // 按端口定位（keep-alive 身份校验的读路径）：命中行 project 必须与调用方一致，
    // 不一致即端口复用/身份不符（协调器降级，不写 activity）。
    let located = store
        .find_active_by_port(port)
        .await
        .expect("find")
        .expect("row");
    assert_eq!(located.project_id, "p8");

    // activity 批量刷盘：GREATEST 单调（旧时间戳不回退）、instance 不匹配 no-op。
    use chrono::{Duration, Utc};
    use shared_types::ActivityFlushEntry;
    let stale = Utc::now() - Duration::hours(1);
    let fresh = Utc::now();
    let touched = store
        .flush_activity(&[ActivityFlushEntry {
            preview_key: "k8".into(),
            instance_id: "inst-8".into(),
            at: stale,
        }])
        .await
        .expect("flush stale");
    assert_eq!(touched, 0, "旧时间戳不得回退 last_activity_at");
    let touched = store
        .flush_activity(&[
            ActivityFlushEntry {
                preview_key: "k8".into(),
                instance_id: "inst-8".into(),
                at: fresh,
            },
            ActivityFlushEntry {
                preview_key: "k8".into(),
                instance_id: "inst-stale".into(),
                at: fresh,
            },
        ])
        .await
        .expect("flush fresh");
    assert_eq!(touched, 1, "只有身份匹配的条目生效");
    let row = store.get("k8").await.unwrap().unwrap();
    assert!(row.last_activity_at >= fresh);

    // 心跳：ready 命中；停止后 None。
    assert!(
        store
            .refresh_heartbeat("k8", "inst-8")
            .await
            .expect("hb")
            .is_some()
    );
    store.accept_stop("k8", "op-stop-8").await.expect("stop");
    let row = store.get("k8").await.unwrap().unwrap();
    store
        .mark_stopped("k8", "op-stop-8", row.revision)
        .await
        .expect("stopped");
    assert!(
        store
            .refresh_heartbeat("k8", "inst-8")
            .await
            .expect("hb")
            .is_none()
    );
}

async fn host_reboot_reconciliation(store: &dyn PreviewLifecycleStore) {
    // 三个实例：同 pod 不同 boot、本 pod 本 boot、他 pod。
    let mk = |key: &str, project: &str, op: &str, instance: &str, uid: &str, boot: &str| {
        let mut input = start_input(key, project, op, instance);
        input.host = host(uid, boot);
        input
    };
    for (key, project, op, instance, uid, boot) in [
        ("k9a", "p9a", "op-9a", "inst-9a", "uid-a", "boot-old"),
        ("k9b", "p9b", "op-9b", "inst-9b", "uid-a", "boot-1"),
        ("k9c", "p9c", "op-9c", "inst-9c", "uid-b", "boot-x"),
    ] {
        let admitted = store
            .accept_start(mk(key, project, op, instance, uid, boot))
            .await
            .expect("accept");
        let AcceptStartOutcome::Admitted(record) = admitted else {
            panic!("must admit")
        };
        store
            .publish_running(key, op, record.revision, 600, record.port.unwrap(), None)
            .await
            .expect("publish");
    }

    let reconciled = store
        .reconcile_host_reboot("uid-a", "boot-1")
        .await
        .expect("reconcile");
    assert_eq!(reconciled.len(), 1, "只收敛同 pod 旧 boot 的行");
    assert_eq!(reconciled[0].preview_key, "k9a");
    assert_eq!(reconciled[0].state, PreviewInstanceState::Stopped);

    // 其余行不受影响。
    let k9b = store.get("k9b").await.unwrap().unwrap();
    assert_eq!(k9b.state, PreviewInstanceState::Ready);
    let k9c = store.get("k9c").await.unwrap().unwrap();
    assert_eq!(k9c.state, PreviewInstanceState::Ready);

    for row in store.list_active().await.expect("list") {
        store
            .mark_failed(
                &row.preview_key,
                &row.instance_id,
                row.revision,
                "test cleanup",
            )
            .await
            .expect("cleanup");
    }
}

async fn failure_then_reaccept(store: &dyn PreviewLifecycleStore) {
    let admitted = store
        .accept_start(start_input("k10", "p10", "op-10", "inst-10"))
        .await
        .expect("accept");
    let AcceptStartOutcome::Admitted(record) = admitted else {
        panic!("must admit")
    };
    store
        .publish_running(
            "k10",
            "op-10",
            record.revision,
            700,
            record.port.unwrap(),
            None,
        )
        .await
        .expect("publish");

    // Ready 探死 → Failed；Failed 可直接重新受理。
    store
        .mark_failed("k10", "inst-10", record.revision, "process exited")
        .await
        .expect("fail");
    let reaccept = store
        .accept_start(start_input("k10", "p10", "op-10b", "inst-10b"))
        .await
        .expect("accept after failed");
    assert!(matches!(reaccept, AcceptStartOutcome::Admitted(_)));
    let row = store.get("k10").await.unwrap().unwrap();
    store
        .mark_failed("k10", &row.instance_id, row.revision, "test cleanup")
        .await
        .expect("cleanup");
}

async fn lookup_views(store: &dyn PreviewLifecycleStore) {
    let admitted = store
        .accept_start(start_input("k11", "p11", "op-11", "inst-11"))
        .await
        .expect("accept");
    let AcceptStartOutcome::Admitted(record) = admitted else {
        panic!("must admit")
    };
    let port = record.port.unwrap();
    store
        .publish_running("k11", "op-11", record.revision, 800, port, None)
        .await
        .expect("publish");

    let by_port = store
        .find_active_by_port(port)
        .await
        .expect("find")
        .expect("row");
    assert_eq!(by_port.preview_key, "k11");
    assert!(store.active_ports().await.expect("ports").contains(&port));

    let mine = store
        .list_by_host("uid-a:boot-1", true)
        .await
        .expect("by host");
    assert!(mine.iter().any(|r| r.preview_key == "k11"));
    let theirs = store
        .list_by_host("uid-z:boot-9", true)
        .await
        .expect("by other host");
    assert!(theirs.is_empty());

    store.accept_stop("k11", "op-stop-11").await.expect("stop");
    let row = store.get("k11").await.unwrap().unwrap();
    store
        .mark_stopped("k11", "op-stop-11", row.revision)
        .await
        .expect("stopped");
    // 停止后端口释放（不再出现在 active 集合）。
    assert!(!store.active_ports().await.expect("ports").contains(&port));
}

/// Late completion cannot succeed merely because a newer generation is stopped;
/// recovery evidence cannot be transplanted to a different unknown instance.
async fn generation_bound_recovery_and_stop(store: &dyn PreviewLifecycleStore) {
    let input = start_input(
        "generationproof",
        "generationproject",
        "generationstart",
        "generationone",
    );
    let AcceptStartOutcome::Admitted(first) = store.accept_start(input).await.unwrap() else {
        panic!("fresh admission expected");
    };
    store
        .mark_unknown(&first.preview_key, &first.instance_id, "host unreachable")
        .await
        .unwrap();
    let mut replacement = start_input(
        "generationproof",
        "generationproject",
        "generationnext",
        "generationtwo",
    );
    replacement.recover_unknown_evidence = Some(shared_types::PreviewRecoveryEvidence {
        instance_id: "wronginstance".into(),
        revision: first.revision,
        detail: "host gone".into(),
    });
    assert_conflict(&store.accept_start(replacement.clone()).await);
    assert_eq!(
        store
            .get(&first.preview_key)
            .await
            .unwrap()
            .unwrap()
            .instance_id,
        first.instance_id
    );
    replacement
        .recover_unknown_evidence
        .as_mut()
        .unwrap()
        .instance_id = first.instance_id.clone();
    let AcceptStartOutcome::Admitted(second) = store.accept_start(replacement).await.unwrap()
    else {
        panic!("identity-bound recovery expected");
    };
    // Startup publication cannot silently change the port already reserved.
    assert_conflict(
        &store
            .publish_running(
                &second.preview_key,
                &second.operation_id,
                second.revision,
                123,
                second.port.unwrap() + 1,
                None,
            )
            .await,
    );
    let stopping = store
        .accept_stop(&second.preview_key, "generationstop")
        .await
        .unwrap();
    let replay = store
        .accept_stop(&second.preview_key, "generationstop")
        .await
        .unwrap();
    assert_eq!(stopping.revision, replay.revision);
    assert_conflict(
        &store
            .accept_stop(&second.preview_key, "differentstop")
            .await,
    );
    let stopped = store
        .mark_stopped(
            &second.preview_key,
            &stopping.operation_id,
            stopping.revision,
        )
        .await
        .unwrap();
    assert_conflict(
        &store
            .mark_stopped(&second.preview_key, "wrongstop", stopping.revision)
            .await,
    );
    assert_eq!(
        store.get(&second.preview_key).await.unwrap().unwrap(),
        stopped
    );
}
