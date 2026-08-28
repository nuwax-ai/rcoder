//! CRUD / session 操作 / RAII 单线程族（从 tests.rs 拆出）。

use super::*;

#[test]
fn test_project_crud() {
    let adapter = make_adapter();
    let project_id = "test-project-1";
    let info = Arc::new(create_test_info(project_id));

    // insert
    adapter
        .insert(project_id.to_string(), info.clone())
        .unwrap();
    assert!(adapter.contains_key(project_id));

    // get
    let retrieved = adapter.get(project_id);
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().project_id(), project_id);

    // remove
    let removed = adapter.remove(project_id);
    assert!(removed.is_some());
    assert!(!adapter.contains_key(project_id));
}

#[test]
fn test_session_operations() {
    let adapter = make_adapter();
    let project_id = "test-project-2";
    let session_id = "test-session-1";

    let info = Arc::new(create_test_info_with_container(project_id, "container-1"));
    adapter.insert(project_id.to_string(), info).unwrap();

    // C1 修复后的推荐路径：add_session_to_project 单步原子
    let added = adapter.add_session_to_project(project_id, session_id);
    assert!(added, "add_session_to_project 应在 project 存在时返回 true");

    let by_session = adapter.get_by_session_id(session_id);
    assert!(by_session.is_some());
    assert_eq!(by_session.unwrap().project_id(), project_id);

    let container_name = adapter.get_container_name_by_session(session_id);
    assert_eq!(container_name, Some("container-1".to_string()));

    // 不存在的 project 应返回 false
    let added2 = adapter.add_session_to_project("nonexistent", session_id);
    assert!(!added2);
}

/// restore_session_to_project：恢复路径不刷 last_activity（idle 计时不因
/// 重启/回源归零），但 session 集合与索引照常登记。
#[test]
fn test_restore_session_preserves_last_activity() {
    let adapter = make_adapter();
    let project_id = "test-project-restore";

    let mut info = create_test_info(project_id);
    let stale = Utc::now() - chrono::Duration::hours(3);
    info.set_timestamps(stale, stale);
    let before = info.last_activity();
    adapter
        .insert(project_id.to_string(), Arc::new(info))
        .unwrap();

    // add 路径：活跃时间推进（用户真实操作语义）。基线显式回拨到过去——
    // 与 Utc::now() 严格比较在时钟同纳秒时会假失败（存量 flaky）
    let mut info2 = create_test_info("other-1");
    let ancient = Utc::now() - chrono::Duration::hours(1);
    info2.set_timestamps(ancient, ancient);
    info2.add_session("s-warmup");
    assert!(
        info2.last_activity() > ancient,
        "add_session must bump last_activity"
    );

    // restore 路径：时间戳不动（boot/回源恢复语义）
    let restored = adapter.restore_session_to_project(project_id, "s-boot");
    assert!(restored, "restore 在 project 存在时应返回 true");
    let got = adapter.get(project_id).unwrap();
    assert_eq!(
        got.last_activity(),
        before,
        "restore must not touch last_activity"
    );
    assert!(
        got.sessions().contains("s-boot"),
        "session set must be restored"
    );

    // 索引登记照常：按 session 键可查
    assert!(adapter.get_by_session_id("s-boot").is_some());

    // 不存在的 project：false 且不写索引（与 add 同防孤儿语义）
    assert!(!adapter.restore_session_to_project("nonexistent", "s-x"));
    assert!(adapter.get_by_session_id("s-x").is_none());
}

#[test]
fn test_iter() {
    let adapter = make_adapter();
    for i in 0..3 {
        let pid = format!("iter-project-{}", i);
        adapter
            .insert(pid.clone(), Arc::new(create_test_info(&pid)))
            .unwrap();
    }
    assert_eq!(adapter.iter().len(), 3);
}

#[test]
fn test_insert_with_session() {
    let adapter = make_adapter();
    let project_id = "test-project-session";
    let session_id = "test-session-abc";
    let info = Arc::new(create_test_info(project_id));

    adapter
        .insert_with_session(project_id.to_string(), info, Some(session_id))
        .unwrap();

    let by_session = adapter.get_by_session_id(session_id);
    assert!(by_session.is_some());
    assert_eq!(by_session.unwrap().project_id(), project_id);

    adapter.clear_session(project_id);
    assert!(adapter.get_by_session_id(session_id).is_none());
}

/// C2 修复后的新语义：多 session 共存（不再覆盖）
///
/// 注意：insert_with_session 接收的 `info` 会覆盖主存储中的 ProjectAndContainerInfo。
/// 生产代码（computer_chat_handler.rs:1123-1132）在调用前会先读出 existing info 并迁移 sessions。
/// 本测试模拟该正确用法。
#[test]
fn test_session_rotation() {
    let adapter = make_adapter();
    let project_id = "test-rotation";
    let info = Arc::new(create_test_info(project_id));

    // 第一次：插入 info 并关联 session-1
    adapter
        .insert_with_session(project_id.to_string(), info.clone(), Some("session-1"))
        .unwrap();
    assert!(adapter.get_by_session_id("session-1").is_some());

    // 模拟生产用法：读出 existing info，迁移已有 sessions，添加新 session
    let mut updated_info = adapter.get(project_id).unwrap().as_ref().clone();
    updated_info.add_session("session-2");
    adapter
        .insert_with_session(
            project_id.to_string(),
            Arc::new(updated_info),
            Some("session-2"),
        )
        .unwrap();

    assert!(adapter.get_by_session_id("session-2").is_some());
    // C2 关键断言：session-1 仍然可查（多窗口场景）
    assert!(
        adapter.get_by_session_id("session-1").is_some(),
        "C2: 新 session 加入后旧 session 应仍可查"
    );

    // latest_session 应指向最新加入的 session-2
    let info = adapter.get(project_id).unwrap();
    assert_eq!(info.latest_session(), Some("session-2"));
    assert_eq!(info.session_count(), 2);
}

/// 多 session：add_session_to_project + clear_session_one 保留其他
#[test]
fn test_multi_session_add_and_clear_one() {
    let adapter = make_adapter();
    let project_id = "test-multi";
    let info = Arc::new(create_test_info(project_id));
    adapter.insert(project_id.to_string(), info).unwrap();

    // 添加 3 个 session
    adapter.add_session_to_project(project_id, "s1");
    adapter.add_session_to_project(project_id, "s2");
    adapter.add_session_to_project(project_id, "s3");

    // 3 个都可查
    assert!(adapter.get_by_session_id("s1").is_some());
    assert!(adapter.get_by_session_id("s2").is_some());
    assert!(adapter.get_by_session_id("s3").is_some());

    let info = adapter.get(project_id).unwrap();
    assert_eq!(info.session_count(), 3);
    assert_eq!(info.latest_session(), Some("s3"));

    // 清单个 session（保留其他）
    let cleared = adapter.clear_session_one(project_id, "s2");
    assert!(cleared, "clear_session_one 应在 session 存在时返回 true");
    assert!(adapter.get_by_session_id("s2").is_none(), "s2 应被清");
    assert!(adapter.get_by_session_id("s1").is_some(), "s1 应保留");
    assert!(adapter.get_by_session_id("s3").is_some(), "s3 应保留");

    let info = adapter.get(project_id).unwrap();
    assert_eq!(info.session_count(), 2);

    // 清不存在的 session 返回 false
    let cleared2 = adapter.clear_session_one(project_id, "nonexistent");
    assert!(!cleared2);
}

/// clear_session（清所有）+ remove_project 自动清理所有 session 索引
#[test]
fn test_clear_all_sessions_and_remove() {
    let adapter = make_adapter();
    let project_id = "test-clear-all";
    let info = Arc::new(create_test_info(project_id));
    adapter.insert(project_id.to_string(), info).unwrap();

    adapter.add_session_to_project(project_id, "s1");
    adapter.add_session_to_project(project_id, "s2");
    assert_eq!(adapter.session_index.len(), 2);

    // clear_session 清所有
    adapter.clear_session(project_id);
    assert_eq!(
        adapter.session_index.len(),
        0,
        "clear_session 应清所有 session 索引"
    );
    let info = adapter.get(project_id).unwrap();
    assert_eq!(info.session_count(), 0);
    assert!(info.latest_session().is_none());

    // 重新添加 + remove 项目
    adapter.add_session_to_project(project_id, "s3");
    adapter.add_session_to_project(project_id, "s4");
    assert_eq!(adapter.session_index.len(), 2);

    drop(adapter.remove(project_id));
    assert_eq!(
        adapter.session_index.len(),
        0,
        "remove 应清理所有 session 索引"
    );
}

/// latest_session 在 remove latest 后自动退化到剩余 session
#[test]
fn test_latest_session_fallback_after_remove() {
    let adapter = make_adapter();
    let project_id = "test-latest-fallback";
    let info = Arc::new(create_test_info(project_id));
    adapter.insert(project_id.to_string(), info).unwrap();

    adapter.add_session_to_project(project_id, "s1");
    adapter.add_session_to_project(project_id, "s2");
    // latest 是 s2

    adapter.clear_session_one(project_id, "s2");
    let info = adapter.get(project_id).unwrap();
    assert_eq!(info.session_count(), 1);
    // latest 退化到剩余的 s1
    assert_eq!(
        info.latest_session(),
        Some("s1"),
        "移除 latest 后应退化到剩余 session"
    );
}

/// 并发压测：8 线程 × 200 轮 add_session + clear_session_one，无 panic/deadlock
#[test]
fn test_concurrent_multi_session_add_and_clear() {
    let (adapter, _rx) =
        ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
    let adapter = Arc::new(adapter);

    const THREADS: usize = 8;
    const ITERS: usize = 200;

    // 预插入 project
    let info = Arc::new(create_test_info("proj-concurrent"));
    adapter.insert("proj-concurrent".to_string(), info).unwrap();

    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = vec![];

    for t in 0..THREADS {
        let adapter = adapter.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            for i in 0..ITERS {
                let sid = format!("t{}-s{}", t, i);
                adapter.add_session_to_project("proj-concurrent", &sid);
                // 50% 概率清掉自己刚加的
                if i % 2 == 0 {
                    adapter.clear_session_one("proj-concurrent", &sid);
                }
            }
        }));
    }

    for h in handles {
        let result = join_with_timeout(h, 15);
        assert!(result.is_some(), "DEADLOCK: concurrent multi-session ops");
    }

    // 最终 session_count 应 = 偶数 iter 数量之和（i%2==1 的没被清）
    let info = adapter.get("proj-concurrent").unwrap();
    let expected: usize = THREADS * (ITERS / 2);
    assert_eq!(
        info.session_count(),
        expected,
        "残留 session 数应等于未清理的数量"
    );
}

#[test]
fn test_len_and_is_empty() {
    let adapter = make_adapter();
    assert!(adapter.is_empty());
    assert_eq!(adapter.len(), 0);

    adapter
        .insert("p1".to_string(), Arc::new(create_test_info("p1")))
        .unwrap();
    assert_eq!(adapter.len(), 1);
    assert!(!adapter.is_empty());
}

#[test]
fn test_update_activity() {
    let adapter = make_adapter();
    let pid = "test-activity";
    adapter
        .insert(pid.to_string(), Arc::new(create_test_info(pid)))
        .unwrap();

    let ts = adapter.update_activity(pid);
    assert!(ts.is_some());

    // 不存在的 project
    assert!(adapter.update_activity("nonexistent").is_none());
}

#[test]
fn test_get_stats() {
    let adapter = make_adapter();
    let stats = adapter.get_stats();
    assert_eq!(stats.total_projects, 0);
    assert_eq!(stats.total_containers, 0);
    assert_eq!(stats.active_sessions, 0);
}

#[test]
fn test_dump_summary() {
    let adapter = make_adapter();
    let summary = adapter.dump_summary();
    assert!(summary.contains("projects=0"));
}

#[test]
fn test_raii_cleanup_on_last_project_remove() {
    let adapter = make_adapter();

    let container = ContainerBasicInfo {
        container_id: "shared-container-id".to_string(),
        container_name: "shared-container".to_string(),
        container_ip: "127.0.0.1".to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: "proj-1".to_string(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: "http://shared".to_string(),
    };

    let mut info1 = ProjectAndContainerInfo::from_parts(
        "proj-1".to_string(),
        Some("user-1".to_string()),
        None,
        None,
        Some(container.clone()),
        ProjectExtendedFields {
            service_type: Some(ServiceType::ComputerAgentRunner),
            ..Default::default()
        },
    );
    info1.set_service_type(Some(ServiceType::ComputerAgentRunner));

    let mut info2 = ProjectAndContainerInfo::from_parts(
        "proj-2".to_string(),
        Some("user-1".to_string()),
        None,
        None,
        Some(container.clone()),
        ProjectExtendedFields {
            service_type: Some(ServiceType::ComputerAgentRunner),
            ..Default::default()
        },
    );
    info2.set_service_type(Some(ServiceType::ComputerAgentRunner));

    adapter
        .insert("proj-1".to_string(), Arc::new(info1))
        .unwrap();
    adapter
        .insert("proj-2".to_string(), Arc::new(info2))
        .unwrap();

    assert_eq!(adapter.containers.len(), 1);

    adapter.remove("proj-1");
    assert_eq!(
        adapter.containers.len(),
        1,
        "容器应保留（ref_count 应 > 0）"
    );

    adapter.remove("proj-2");
    // 新行为:ref_count=0 不立即清容器条目(保留复用,交 cleaner idle 回收),故仍为 1。
    assert_eq!(
        adapter.containers.len(),
        1,
        "ref_count=0 保留容器条目待 cleaner 回收(不立即销毁)"
    );
    assert_eq!(
        adapter
            .containers
            .get("shared-container")
            .unwrap()
            .ref_count(),
        0,
        "ref_count 应归零"
    );
}

#[test]
fn test_reinsert_same_project_no_ref_leak() {
    let adapter = make_adapter();

    let info = Arc::new(create_test_info_with_container("proj-A", "container-A"));

    adapter.insert("proj-A".to_string(), info.clone()).unwrap();
    assert_eq!(adapter.containers.len(), 1);

    adapter.insert("proj-A".to_string(), info.clone()).unwrap();
    assert_eq!(adapter.containers.len(), 1);

    adapter.remove("proj-A");
    assert_eq!(
        adapter.containers.len(),
        1,
        "ref_count=0 保留容器条目(待 cleaner 回收);重复 insert 未致 ref_count 泄露"
    );
}

#[test]
fn test_save_container_update() {
    let adapter = make_adapter();

    // containers DashMap 以 container_name 为键；save_container 与 insert 都用 container_name，
    // 故此处 container_name 与 insert 的 info.container().container_name 必须一致才能命中同一条目。
    let container = ContainerBasicInfo {
        container_id: "proj-1".to_string(),
        container_name: "save-test".to_string(),
        container_ip: "10.0.0.1".to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: "proj-1".to_string(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: "http://test".to_string(),
    };

    // 第一次 save：创建新条目（ref_count=0）
    adapter
        .save_container(&container, Some(ServiceType::WebAgentRunner))
        .unwrap();
    assert_eq!(adapter.containers.len(), 1);

    // 通过 project insert 关联容器（ref_count 0→1）
    let mut info = create_test_info("proj-1");
    info.set_container(Some(container.clone()));
    adapter
        .insert("proj-1".to_string(), Arc::new(info))
        .unwrap();

    // 验证 ref_count = 1（键为 container_name "save-test"）
    let ce = adapter.containers.get("save-test").unwrap();
    assert_eq!(ce.value().ref_count(), 1);

    // 第二次 save：更新已有条目（保持 container_name 不变以命中同一条目），ref_count 应保持不变
    let mut updated_container = container.clone();
    updated_container.container_ip = "10.0.0.2".to_string();
    adapter
        .save_container(&updated_container, Some(ServiceType::ComputerAgentRunner))
        .unwrap();

    let ce = adapter.containers.get("save-test").unwrap();
    assert_eq!(
        ce.value().ref_count(),
        1,
        "save_container 更新不应改变 ref_count"
    );
    assert_eq!(ce.value().info().container_ip, "10.0.0.2");
    assert_eq!(ce.value().service_type(), ServiceType::ComputerAgentRunner);
}

// ========== 并发 RAII + 死锁验证测试 ==========

use std::sync::Barrier;
use std::thread;
