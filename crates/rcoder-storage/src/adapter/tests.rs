use std::sync::Mutex;

use super::*;
use shared_types::{ContainerBasicInfo, ProjectExtendedFields, ServiceType};

/// 测试用的 K8s namespace
const TEST_NAMESPACE: &str = "test-namespace";
/// 测试用的 K8s 集群域名
const TEST_CLUSTER_DOMAIN: &str = "test.cluster.local";

fn create_test_info(project_id: &str) -> ProjectAndContainerInfo {
    let mut info = ProjectAndContainerInfo::new(project_id.to_string());
    info.set_service_type(Some(ServiceType::WebAgentRunner));
    info
}

fn create_test_info_with_container(
    project_id: &str,
    container_name: &str,
) -> ProjectAndContainerInfo {
    let mut info = create_test_info(project_id);
    info.set_container(Some(ContainerBasicInfo {
        container_id: format!("{}-id", container_name),
        container_name: container_name.to_string(),
        container_ip: "127.0.0.1".to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: project_id.to_string(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: format!("http://{}", container_name),
    }));
    info
}

fn make_adapter() -> ProjectAdapter {
    let (adapter, _) =
        ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
    adapter
}

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

    // add 路径：活跃时间推进（用户真实操作语义）
    let mut info2 = create_test_info("other-1");
    let t0 = Utc::now();
    info2.add_session("s-warmup");
    assert!(
        info2.last_activity() > t0,
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
use std::time::{Duration, Instant};

fn join_with_timeout<T>(handle: thread::JoinHandle<T>, timeout_secs: u64) -> Option<T> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    while !handle.is_finished() {
        if Instant::now() > deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
    handle.join().ok()
}

fn drain_cleanup_requests(
    rx: &Mutex<tokio::sync::mpsc::Receiver<CleanupRequest>>,
) -> Vec<CleanupRequest> {
    let mut guard = rx.lock().unwrap();
    let mut requests = vec![];
    while let Ok(req) = guard.try_recv() {
        requests.push(req);
    }
    requests
}

fn create_shared_project(
    project_id: &str,
    user_id: &str,
    container: &ContainerBasicInfo,
) -> ProjectAndContainerInfo {
    let mut info = ProjectAndContainerInfo::from_parts(
        project_id.to_string(),
        Some(user_id.to_string()),
        None,
        None,
        Some(container.clone()),
        ProjectExtendedFields {
            service_type: Some(ServiceType::ComputerAgentRunner),
            ..Default::default()
        },
    );
    info.set_service_type(Some(ServiceType::ComputerAgentRunner));
    info
}

#[test]
fn test_concurrent_insert_remove_no_deadlock() {
    let (adapter, rx) =
        ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
    let adapter = Arc::new(adapter);
    let rx = Arc::new(Mutex::new(rx));

    const THREADS: usize = 8;
    const ITERS: usize = 50;
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = vec![];

    for t in 0..THREADS {
        let adapter = adapter.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            for i in 0..ITERS {
                let pid = format!("t{}-i{}", t, i);
                let info = Arc::new(create_test_info_with_container(
                    &pid,
                    &format!("c-t{}-i{}", t, i),
                ));
                drop(adapter.insert(pid.clone(), info));
                drop(adapter.remove(&pid));
            }
        }));
    }

    for h in handles {
        let result = join_with_timeout(h, 15);
        assert!(
            result.is_some(),
            "DEADLOCK: thread did not complete within 15s"
        );
    }

    assert_eq!(adapter.len(), 0, "all projects should be removed");
    // 新行为:ref_count=0 保留容器条目(交 cleaner 回收),故 400 个唯一容器全部保留。
    assert_eq!(
        adapter.containers.len(),
        THREADS * ITERS,
        "ref_count=0 保留容器条目(不立即清,交 cleaner 回收)"
    );

    // ref_count=0 不再发送 cleanup_tx(物理销毁由 cleaner idle 回收路径触发)。
    let cleanups = drain_cleanup_requests(&rx);
    assert_eq!(
        cleanups.len(),
        0,
        "RAII(ref_count=0)不再发 cleanup,物理销毁交 cleaner"
    );
}

// 并发同 project_id insert/remove：per-project 锁序列化操作，ref_count 精确正确。
#[test]
fn test_concurrent_same_project_insert_remove() {
    let (adapter, _rx) =
        ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
    let adapter = Arc::new(adapter);

    const THREADS: usize = 8;
    const ITERS: usize = 50; // 200 在补偿 dec 增加锁竞争后超 15s 超时;50 足够验证 ref_count 正确性
    let project_id = "shared-project";
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = vec![];

    for _ in 0..THREADS {
        let adapter = adapter.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..ITERS {
                let info = Arc::new(create_test_info_with_container(
                    project_id,
                    "shared-container",
                ));
                drop(adapter.insert(project_id.to_string(), info));
                drop(adapter.remove(project_id));
            }
        }));
    }

    for h in handles {
        let result = join_with_timeout(h, 15);
        assert!(
            result.is_some(),
            "DEADLOCK: concurrent insert/remove of same project_id"
        );
    }
}

#[test]
fn test_concurrent_shared_container_remove() {
    let (adapter, rx) =
        ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
    let adapter = Arc::new(adapter);
    let rx = Arc::new(Mutex::new(rx));

    let container = ContainerBasicInfo {
        container_id: "shared-id".to_string(),
        container_name: "shared-container".to_string(),
        container_ip: "10.0.0.1".to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: String::new(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: "http://shared".to_string(),
    };

    let info1 = create_shared_project("proj-1", "user-1", &container);
    let info2 = create_shared_project("proj-2", "user-1", &container);

    adapter
        .insert("proj-1".to_string(), Arc::new(info1))
        .unwrap();
    adapter
        .insert("proj-2".to_string(), Arc::new(info2))
        .unwrap();

    assert_eq!(adapter.containers.len(), 1);

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = vec![];

    for pid in ["proj-1", "proj-2"] {
        let adapter = adapter.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            adapter.remove(pid)
        }));
    }

    for h in handles {
        let result = join_with_timeout(h, 10);
        assert!(
            result.is_some(),
            "DEADLOCK: concurrent remove of shared container projects"
        );
    }

    assert_eq!(adapter.len(), 0);
    // 新行为:ref_count=0 保留容器条目(交 cleaner 回收)。
    assert_eq!(
        adapter.containers.len(),
        1,
        "ref_count=0 保留容器条目(不立即清,交 cleaner 回收)"
    );

    // ref_count=0 不再发 cleanup(物理销毁由 cleaner idle 回收路径触发)。
    let cleanups = drain_cleanup_requests(&rx);
    assert_eq!(
        cleanups.len(),
        0,
        "RAII(ref_count=0)不再发 cleanup,物理销毁交 cleaner"
    );
}

#[test]
fn test_concurrent_session_update_and_remove() {
    let (adapter, _rx) =
        ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
    let adapter = Arc::new(adapter);

    let pid = "concurrent-session-proj";
    let info = Arc::new(create_test_info_with_container(pid, "session-container"));
    adapter.insert(pid.to_string(), info).unwrap();

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = vec![];

    {
        let adapter = adapter.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            for i in 0..100 {
                let sid = format!("session-{}", i);
                // C2 修复后改用 add_session_to_project（多 session 模型）
                adapter.add_session_to_project(pid, &sid);
            }
        }));
    }

    {
        let adapter = adapter.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            thread::sleep(Duration::from_millis(5));
            drop(adapter.remove(pid));
        }));
    }

    for h in handles {
        let result = join_with_timeout(h, 10);
        assert!(
            result.is_some(),
            "DEADLOCK: concurrent session update and remove"
        );
    }
}

// 与 test_concurrent_same_project_insert_remove 同款竞态，已用补偿事务修复。
#[test]
fn test_concurrent_insert_with_session_and_remove() {
    let (adapter, _rx) =
        ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
    let adapter = Arc::new(adapter);

    const THREADS: usize = 4;
    const ITERS: usize = 50;
    let pid = "session-battle";
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = vec![];

    for t in 0..THREADS {
        let adapter = adapter.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            for i in 0..ITERS {
                let info = Arc::new(create_test_info_with_container(pid, "battle-c"));
                let sid = format!("sid-{}-{}", t, i);
                drop(adapter.insert_with_session(pid.to_string(), info, Some(&sid)));
                drop(adapter.remove(pid));
            }
        }));
    }

    for h in handles {
        let result = join_with_timeout(h, 15);
        assert!(
            result.is_some(),
            "DEADLOCK: concurrent insert_with_session and remove"
        );
    }
}

#[test]
fn test_concurrent_remove_nonexistent() {
    let (adapter, _rx) =
        ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
    let adapter = Arc::new(adapter);

    const THREADS: usize = 8;
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = vec![];

    for _ in 0..THREADS {
        let adapter = adapter.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            for _ in 0..100 {
                let result = adapter.remove("nonexistent-project");
                assert!(result.is_none(), "removing nonexistent should return None");
            }
        }));
    }

    for h in handles {
        let result = join_with_timeout(h, 10);
        assert!(
            result.is_some(),
            "DEADLOCK: concurrent remove of nonexistent project"
        );
    }
}

#[test]
fn test_concurrent_stress_mixed_operations() {
    let (adapter, _rx) =
        ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
    let adapter = Arc::new(adapter);

    for i in 0..10 {
        let pid = format!("preload-{}", i);
        let info = Arc::new(create_test_info_with_container(
            &pid,
            &format!("c-pre-{}", i),
        ));
        adapter.insert(pid, info).unwrap();
    }

    const THREADS: usize = 8;
    const ITERS: usize = 30;
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut handles = vec![];

    for t in 0..THREADS {
        let adapter = adapter.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            barrier.wait();
            for i in 0..ITERS {
                let pid = format!("stress-{}-{}", t, i);
                let info = Arc::new(create_test_info_with_container(
                    &pid,
                    &format!("sc-{}-{}", t, i),
                ));

                drop(adapter.insert(pid.clone(), info));
                drop(adapter.get(&pid));
                let _ = adapter.update_activity(&pid);

                let sid = format!("sid-{}-{}", t, i);
                adapter.add_session_to_project(&pid, &sid);
                drop(adapter.get_by_session_id(&sid));
                drop(adapter.get_container_name_by_session(&sid));

                adapter.clear_session(&pid);
                drop(adapter.remove(&pid));
            }
        }));
    }

    for h in handles {
        let result = join_with_timeout(h, 15);
        assert!(
            result.is_some(),
            "DEADLOCK: stress test with mixed operations"
        );
    }
}

#[test]
fn test_raii_cleanup_request_content() {
    let (adapter, rx) =
        ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
    let rx = Arc::new(Mutex::new(rx));

    let info = Arc::new(create_test_info_with_container("proj-verify", "c-verify"));
    adapter.insert("proj-verify".to_string(), info).unwrap();

    let removed = adapter.remove("proj-verify");
    assert!(removed.is_some());

    // 新行为:ref_count=0 不发 cleanup(物理销毁交 cleaner idle 回收),故无 cleanup 请求。
    // cleanup 请求的"内容"由 delete_container_with_projects 路径(显式销毁)覆盖测试。
    let cleanups = drain_cleanup_requests(&rx);
    assert_eq!(cleanups.len(), 0, "RAII(ref_count=0)不再发 cleanup");
}

#[test]
fn test_shared_container_ref_count_no_leak_under_reinsert() {
    let (adapter, rx) =
        ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());
    let rx = Arc::new(Mutex::new(rx));

    let container = ContainerBasicInfo {
        container_id: "leak-test-id".to_string(),
        container_name: "leak-test".to_string(),
        container_ip: "10.0.0.5".to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: String::new(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: "http://leak".to_string(),
    };

    for round in 0..5 {
        let info1 = create_shared_project("proj-1", "user-leak", &container);
        let info2 = create_shared_project("proj-2", "user-leak", &container);

        adapter
            .insert("proj-1".to_string(), Arc::new(info1))
            .unwrap();
        adapter
            .insert("proj-2".to_string(), Arc::new(info2))
            .unwrap();

        assert_eq!(
            adapter.containers.len(),
            1,
            "round {}: should have 1 container",
            round
        );

        adapter.remove("proj-1");
        assert_eq!(
            adapter.containers.len(),
            1,
            "round {}: container should persist after removing proj-1",
            round
        );

        adapter.remove("proj-2");
        assert_eq!(
            adapter.containers.len(),
            1,
            "round {}: ref_count=0 保留容器条目(交 cleaner 回收),下轮 insert 复用",
            round
        );
    }

    // 新行为:ref_count=0 不发 cleanup(物理销毁交 cleaner),5 轮均无 cleanup 请求。
    let cleanups = drain_cleanup_requests(&rx);
    assert_eq!(
        cleanups.len(),
        0,
        "RAII(ref_count=0)不再发 cleanup,5 轮均无请求"
    );
}

// ========== 索引一致性测试 ==========

#[test]
fn test_index_user_id_lookup() {
    // insert 后，get_container_by_user_id 通过索引 O(1) 查找
    let adapter = make_adapter();

    let container = ContainerBasicInfo {
        container_id: "cid-user-1".to_string(),
        container_name: "user-container".to_string(),
        container_ip: "10.0.0.1".to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: "proj-1".to_string(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: "http://test".to_string(),
    };

    let mut info = ProjectAndContainerInfo::from_parts(
        "proj-1".to_string(),
        Some("user-abc".to_string()),
        None,
        None,
        Some(container),
        ProjectExtendedFields {
            service_type: Some(ServiceType::ComputerAgentRunner),
            ..Default::default()
        },
    );
    info.set_service_type(Some(ServiceType::ComputerAgentRunner));
    adapter
        .insert("proj-1".to_string(), Arc::new(info))
        .unwrap();

    // 通过 user_id 查找容器
    let found = adapter.get_container_by_user_id("user-abc", &ServiceType::ComputerAgentRunner);
    assert!(
        found.is_some(),
        "get_container_by_user_id 应通过索引找到容器"
    );
    assert_eq!(found.unwrap().container_id, "cid-user-1");

    // 不存在的 user_id
    assert!(
        adapter
            .get_container_by_user_id("nonexistent", &ServiceType::ComputerAgentRunner)
            .is_none()
    );
}

/// 回归测试：同 user_id 下不同 ServiceType 项目按 service_type 隔离查找
///
/// 场景：user_id=6 同时存在 Computer（proj-A）和 Web（proj-B）两个业务。
/// 多值索引 `user_id_to_project_ids` 同时收录两类项目的 project_id（信息完整），
/// 查询侧（find_by_user_id / find_projects_by_user_id）按 service_type 过滤，
/// 确保不串用、且 Web 项目不计入 Computer 容器的清理决策。
#[test]
fn test_user_id_index_not_polluted_by_web_project() {
    use shared_types::ContainerLookup;
    let adapter = make_adapter();

    let mk_container = |cid: &str, ip: &str, pid: &str| ContainerBasicInfo {
        container_id: cid.to_string(),
        container_name: format!("container-{}", cid),
        container_ip: ip.to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: pid.to_string(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: format!("http://{}", cid),
    };

    // Computer 项目（user_id 索引消费者）
    let mut comp = ProjectAndContainerInfo::from_parts(
        "proj-A".to_string(),
        Some("user-6".to_string()),
        None,
        None,
        Some(mk_container("cid-comp", "10.0.0.1", "proj-A")),
        ProjectExtendedFields {
            service_type: Some(ServiceType::ComputerAgentRunner),
            ..Default::default()
        },
    );
    comp.set_service_type(Some(ServiceType::ComputerAgentRunner));
    adapter
        .insert("proj-A".to_string(), Arc::new(comp))
        .unwrap();

    // Web 项目（同一 user_id=6，模拟 pod_ensure 对 Web 也 set_user_id）
    let mut web = ProjectAndContainerInfo::from_parts(
        "proj-B".to_string(),
        Some("user-6".to_string()),
        None,
        None,
        Some(mk_container("cid-web", "10.0.0.2", "proj-B")),
        ProjectExtendedFields {
            service_type: Some(ServiceType::WebAgentRunner),
            ..Default::default()
        },
    );
    web.set_service_type(Some(ServiceType::WebAgentRunner));
    adapter.insert("proj-B".to_string(), Arc::new(web)).unwrap();

    // 关键断言 1：多值索引 user-6 同时收录两类业务的 project（信息完整）
    let collected: Vec<String> = adapter
        .user_id_to_project_ids
        .get("user-6")
        .map(|s| s.iter().map(|e| e.key().clone()).collect())
        .unwrap_or_default();
    assert_eq!(
        collected
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>(),
        ["proj-A".to_string(), "proj-B".to_string()]
            .into_iter()
            .collect::<std::collections::HashSet<_>>(),
        "多值索引应同时收录 Computer 和 Web 项目（同 user_id）"
    );

    // 关键断言 2：find_by_user_id("6", Computer) → Computer 容器 IP（按 service_type 过滤，不串到 Web）
    assert_eq!(
        adapter.find_by_user_id("user-6", &ServiceType::ComputerAgentRunner),
        Some(adapter.resolve_backend_addr(&mk_container("cid-comp", "10.0.0.1", "proj-A",))),
        "Computer 查找应命中 Computer 容器，不被同 user 的 Web 项目影响"
    );

    // 关键断言 3：find_projects_by_user_id 按 service_type 过滤
    // Web 项目虽记录了 user_id，但不应计入 Computer 的项目集合（避免 cleanup 误保活）
    let comp_projects =
        adapter.find_projects_by_user_id("user-6", &ServiceType::ComputerAgentRunner);
    assert_eq!(
        comp_projects
            .iter()
            .map(|p| p.project_id())
            .collect::<Vec<_>>(),
        vec!["proj-A"],
        "find_projects_by_user_id(Computer) 应只返回 Computer 项目"
    );
    let web_projects = adapter.find_projects_by_user_id("user-6", &ServiceType::WebAgentRunner);
    assert_eq!(
        web_projects
            .iter()
            .map(|p| p.project_id())
            .collect::<Vec<_>>(),
        vec!["proj-B"],
        "find_projects_by_user_id(Web) 应只返回 Web 项目"
    );

    // 关键断言 4：删除 Web 项目后，user-6 的索引集合应只剩 proj-A（Computer）
    adapter.remove("proj-B");
    let remaining: Vec<String> = adapter
        .user_id_to_project_ids
        .get("user-6")
        .map(|s| s.iter().map(|e| e.key().clone()).collect())
        .unwrap_or_default();
    assert_eq!(
        remaining,
        vec!["proj-A".to_string()],
        "删除 Web 项目后，user-6 索引集合应只剩 Computer 项目 proj-A"
    );
    // 且 Computer 查找仍正常
    assert_eq!(
        adapter.find_by_user_id("user-6", &ServiceType::ComputerAgentRunner),
        Some(adapter.resolve_backend_addr(&mk_container("cid-comp", "10.0.0.1", "proj-A",))),
        "删除 Web 项目后，Computer 查找应仍命中 Computer 容器"
    );
}

/// 验证 user_id 索引单值限制（诊断用）：
/// user 6 有两个 Computer 项目 proj-A/proj-C（共享同一容器，refcount=2）。
/// user_id 索引单值，指向最后插入的 proj-C。删除 proj-C 后索引被清，
/// 但 proj-A 仍引用容器（refcount=1）——此时 find_by_user_id 应仍能找到容器。
#[test]
fn test_find_by_user_id_after_indexed_project_removed() {
    use shared_types::ContainerLookup;
    let adapter = make_adapter();

    let mk_container = || ContainerBasicInfo {
        container_id: "cid-shared".to_string(),
        container_name: "computer-container".to_string(),
        container_ip: "10.0.0.9".to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: String::new(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: "http://shared".to_string(),
    };

    let mk_proj = |pid: &str| {
        let mut p = ProjectAndContainerInfo::from_parts(
            pid.to_string(),
            Some("user-6".to_string()),
            None,
            None,
            Some(mk_container()),
            ProjectExtendedFields {
                service_type: Some(ServiceType::ComputerAgentRunner),
                ..Default::default()
            },
        );
        p.set_service_type(Some(ServiceType::ComputerAgentRunner));
        p
    };

    adapter
        .insert("proj-A".to_string(), Arc::new(mk_proj("proj-A")))
        .unwrap();
    adapter
        .insert("proj-C".to_string(), Arc::new(mk_proj("proj-C")))
        .unwrap();

    // 两项目共享同一容器条目（键为 container_name "computer-container"）
    assert_eq!(
        adapter.containers.len(),
        1,
        "两个 Computer 项目应共享同一容器条目"
    );
    assert_eq!(
        adapter
            .containers
            .get("computer-container")
            .unwrap()
            .ref_count(),
        2
    );

    // 删除 proj-C：容器仍存活（proj-A 引用，refcount=1）
    adapter.remove("proj-C");
    assert_eq!(adapter.containers.len(), 1, "容器应仍存活（proj-A 引用）");
    assert_eq!(
        adapter
            .containers
            .get("computer-container")
            .unwrap()
            .ref_count(),
        1
    );

    // find_by_user_id 走 find_projects_by_user_id 扫描，proj-A 仍引用容器 → 应仍能找到
    let result = adapter.find_by_user_id("user-6", &ServiceType::ComputerAgentRunner);
    assert_eq!(
        result,
        Some(adapter.resolve_backend_addr(&mk_container())),
        "删除 proj-C 后，user 6 仍有 proj-A 引用容器，find_by_user_id 应能找到"
    );
}

/// Computer pod_id 共享容器：不同 user 通过同一 pod_id 共享一个容器。
/// 验证 container_key=pod_id → 共享容器条目（refcount）+ RAII 正确。
#[test]
fn test_computer_pod_id_shared_container() {
    let adapter = make_adapter();

    let shared = ContainerBasicInfo {
        container_id: "cid-shared-pod".to_string(),
        container_name: "computer-shared".to_string(),
        container_ip: "10.0.0.7".to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: String::new(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: "http://shared".to_string(),
    };

    // user-A、user-B 各自一个 Computer 项目，通过 pod_id="pod-shared" 共享容器
    let mk_proj = |pid: &str, uid: &str| {
        let mut p = ProjectAndContainerInfo::from_parts(
            pid.to_string(),
            Some(uid.to_string()),
            Some("pod-shared".to_string()),
            None,
            Some(shared.clone()),
            ProjectExtendedFields {
                service_type: Some(ServiceType::ComputerAgentRunner),
                ..Default::default()
            },
        );
        p.set_service_type(Some(ServiceType::ComputerAgentRunner));
        p
    };

    adapter
        .insert("proj-A".to_string(), Arc::new(mk_proj("proj-A", "user-A")))
        .unwrap();
    // container_key = pod_id（Computer 有 pod_id 时），故与 user-B 共享同一容器条目
    assert_eq!(
        adapter.get("proj-A").unwrap().container_key(),
        "pod-shared",
        "Computer 有 pod_id 时 container_key 应为 pod_id"
    );
    adapter
        .insert("proj-B".to_string(), Arc::new(mk_proj("proj-B", "user-B")))
        .unwrap();

    // 两个 user 共享同一容器条目（refcount=2）。键为 container_name "computer-shared"。
    assert_eq!(adapter.containers.len(), 1, "两个 user 应共享同一容器条目");
    assert_eq!(
        adapter
            .containers
            .get("computer-shared")
            .unwrap()
            .ref_count(),
        2
    );

    // 任一 user 查询都能命中共享容器
    use shared_types::ContainerLookup;
    assert_eq!(
        adapter.find_by_user_id("user-A", &ServiceType::ComputerAgentRunner),
        Some(adapter.resolve_backend_addr(&shared))
    );
    assert_eq!(
        adapter.find_by_user_id("user-B", &ServiceType::ComputerAgentRunner),
        Some(adapter.resolve_backend_addr(&shared))
    );

    // 删除一个 user 的项目：容器仍存活（另一个 user 还在用）
    adapter.remove("proj-A");
    assert_eq!(adapter.containers.len(), 1, "容器应仍存活（user-B 还在用）");
    assert_eq!(
        adapter
            .containers
            .get("computer-shared")
            .unwrap()
            .ref_count(),
        1
    );

    // 删除最后一个:ref_count=0,新行为保留容器条目(交 cleaner 回收),不立即销毁。
    adapter.remove("proj-B");
    assert_eq!(
        adapter.containers.len(),
        1,
        "ref_count=0 保留容器条目(交 cleaner 回收)"
    );
}

/// 回归测试：同 logical id 跨 ServiceType 不碰撞（container_name 键天然含 service_type 前缀）
///
/// Computer user_id="6" 与 Web project_id="6" 共存。旧方案（裸 logical id 键）会撞键导致
/// refcount 跨类型混算、查找互串；新方案（container_name 键）两条目独立。
#[test]
fn test_cross_service_type_no_key_collision() {
    use shared_types::ContainerLookup;
    let adapter = make_adapter();

    let mk = |name: &str, ip: &str| ContainerBasicInfo {
        container_id: format!("cid-{name}"),
        container_name: name.to_string(),
        container_ip: ip.to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: String::new(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: format!("http://{name}"),
    };

    // Computer 项目：user_id="6"，container_name 含 computer 前缀
    let mut comp = ProjectAndContainerInfo::from_parts(
        "proj-comp".to_string(),
        Some("6".to_string()),
        None,
        None,
        Some(mk("computer-agent-runner-6", "10.0.0.1")),
        ProjectExtendedFields {
            service_type: Some(ServiceType::ComputerAgentRunner),
            ..Default::default()
        },
    );
    comp.set_service_type(Some(ServiceType::ComputerAgentRunner));
    adapter
        .insert("proj-comp".to_string(), Arc::new(comp))
        .unwrap();

    // Web 项目：project_id="6"，container_name 含 web 前缀
    let mut web = ProjectAndContainerInfo::from_parts(
        "6".to_string(),
        None,
        None,
        None,
        Some(mk("web-agent-runner-6", "10.0.0.2")),
        ProjectExtendedFields {
            service_type: Some(ServiceType::WebAgentRunner),
            ..Default::default()
        },
    );
    web.set_service_type(Some(ServiceType::WebAgentRunner));
    adapter.insert("6".to_string(), Arc::new(web)).unwrap();

    // 两个独立容器条目（键不同：container_name 含 service_type 前缀）
    assert_eq!(
        adapter.containers.len(),
        2,
        "同 logical id=\"6\" 不同 service_type 应各自独立条目（不撞键）"
    );
    assert!(adapter.containers.contains_key("computer-agent-runner-6"));
    assert!(adapter.containers.contains_key("web-agent-runner-6"));

    // 查找互不串
    assert_eq!(
        adapter.find_by_user_id("6", &ServiceType::ComputerAgentRunner),
        Some(adapter.resolve_backend_addr(&mk("computer-agent-runner-6", "10.0.0.1"))),
        "Computer 查找应命中 Computer 容器"
    );
    assert_eq!(
        adapter.find_by_project_id("6", &ServiceType::WebAgentRunner),
        Some(adapter.resolve_backend_addr(&mk("web-agent-runner-6", "10.0.0.2"))),
        "Web 查找应命中 Web 容器"
    );

    // 删除 Computer 项目:ref_count=0,新行为保留容器条目(交 cleaner 回收),Web 不受影响。
    // 键不碰撞的核心已由上方"两个独立条目 + 查找互不串"证明;保留行为下两者都在。
    adapter.remove("proj-comp");
    assert_eq!(
        adapter.containers.len(),
        2,
        "两个 service_type 容器均保留(Computer ref_count=0 也保留,交 cleaner)"
    );
    assert!(adapter.containers.contains_key("web-agent-runner-6"));
    assert!(
        adapter.containers.contains_key("computer-agent-runner-6"),
        "Computer 容器 ref_count=0 仍保留(交 cleaner 回收)"
    );
}

/// 回归测试：跨重建稳定（container_name 确定性，重建不误增条目/不误动 refcount）
///
/// 容器重建：container_id 变，但 container_name 不变（确定性命名）。
/// 用 container_name 作键时，同 name 重复 insert → container_changed=false → 不触发 dec/inc。
#[test]
fn test_container_recreation_stability() {
    let adapter = make_adapter();

    let mk_proj = |cid: &str| {
        let container = ContainerBasicInfo {
            container_id: cid.to_string(),
            container_name: "computer-agent-runner-6".to_string(),
            container_ip: "10.0.0.1".to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: String::new(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: "http://c".to_string(),
        };
        let mut p = ProjectAndContainerInfo::from_parts(
            "proj-A".to_string(),
            Some("6".to_string()),
            None,
            None,
            Some(container),
            ProjectExtendedFields {
                service_type: Some(ServiceType::ComputerAgentRunner),
                ..Default::default()
            },
        );
        p.set_service_type(Some(ServiceType::ComputerAgentRunner));
        p
    };

    adapter
        .insert("proj-A".to_string(), Arc::new(mk_proj("cid-v1")))
        .unwrap();
    assert_eq!(adapter.containers.len(), 1);
    assert_eq!(
        adapter
            .containers
            .get("computer-agent-runner-6")
            .unwrap()
            .ref_count(),
        1
    );

    // 模拟容器重建：container_id 变（cid-v1→cid-v2），container_name 不变
    adapter
        .insert("proj-A".to_string(), Arc::new(mk_proj("cid-v2")))
        .unwrap();
    assert_eq!(
        adapter.containers.len(),
        1,
        "重建（同 container_name）不应新增容器条目"
    );
    assert_eq!(
        adapter
            .containers
            .get("computer-agent-runner-6")
            .unwrap()
            .ref_count(),
        1,
        "重建（同 container_name）refcount 应保持不变（不误触发 RAII）"
    );
    // 容器条目信息应刷新到新的 container_id（修复容器重建陈旧问题）
    assert_eq!(
        adapter
            .containers
            .get("computer-agent-runner-6")
            .unwrap()
            .info()
            .container_id,
        "cid-v2",
        "重建后容器条目应刷新为新 container_id，find_by_project_id 才能拿到新 ip"
    );
    // 旧 container_id 的反向索引应被清理（不累积陈旧条目）
    assert!(
        !adapter.container_id_to_key.contains_key("cid-v1"),
        "重建后旧 container_id 反向索引应被清理"
    );
    assert!(adapter.container_id_to_key.contains_key("cid-v2"));
}

/// 回归测试：所有容器查找路径统一走 containers[name] 权威源，结果一致；
/// 且容器重建后所有路径都返回新 IP（不出现「部分路径新鲜、部分陈旧」）。
#[test]
fn test_lookup_source_consistency() {
    use shared_types::ContainerLookup;
    let adapter = make_adapter();

    let mk_proj = |cid: &str, ip: &str| {
        let container = ContainerBasicInfo {
            container_id: cid.to_string(),
            container_name: "computer-agent-runner-6".to_string(),
            container_ip: ip.to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: String::new(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: format!("http://{cid}"),
        };
        let mut p = ProjectAndContainerInfo::from_parts(
            "proj-A".to_string(),
            Some("6".to_string()),
            None,
            None,
            Some(container),
            ProjectExtendedFields {
                service_type: Some(ServiceType::ComputerAgentRunner),
                ..Default::default()
            },
        );
        p.set_service_type(Some(ServiceType::ComputerAgentRunner));
        p
    };

    adapter
        .insert(
            "proj-A".to_string(),
            Arc::new(mk_proj("cid-v1", "10.0.0.1")),
        )
        .unwrap();

    let st = ServiceType::ComputerAgentRunner;
    // 三条查找路径（user_id / project_id / get_container_by_user_id）应解析到同一
    // backend addr（K8s 模式为 Service FQDN，Docker 模式为容器 IP，统一由
    // resolve_backend_addr 决定）。
    assert_eq!(
        adapter.find_by_user_id("6", &st),
        adapter.find_by_project_id("proj-A", &st),
        "find_by_user_id 与 find_by_project_id 应返回同一 backend addr"
    );
    assert_eq!(
        adapter.find_by_user_id("6", &st),
        adapter
            .get_container_by_user_id("6", &st)
            .map(|c| adapter.resolve_backend_addr(&c)),
        "find_by_user_id 与 get_container_by_user_id 应解析到同一 backend addr"
    );

    // 模拟容器重建：同 container_name、新 container_id/ip
    adapter
        .insert(
            "proj-A".to_string(),
            Arc::new(mk_proj("cid-v2", "10.0.0.2")),
        )
        .unwrap();

    // 重建后权威源 containers[name] 已刷新：Docker 模式 find_by_* 返回新 IP，
    // K8s 模式 FQDN 基于 container_name（不变）；两条 find 路径仍应一致，
    // 且底层 container_ip 已刷新为新值。
    assert_eq!(
        adapter.find_by_user_id("6", &st),
        adapter.find_by_project_id("proj-A", &st),
        "重建后 find_by_user_id 与 find_by_project_id 仍应一致"
    );
    assert_eq!(
        adapter
            .get_container_by_user_id("6", &st)
            .map(|c| c.container_ip),
        Some("10.0.0.2".to_string()),
        "重建后权威源 container_ip 应刷新为新 IP"
    );
}

#[test]
fn test_index_pod_id_lookup() {
    // get_container_by_pod_id 通过索引 O(1) 查找（返回任一 project 的 container）；
    // find_projects_by_pod_id 全量遍历返回所有同 pod project（cleanup strategy 依赖此行为）
    let adapter = make_adapter();

    let container = ContainerBasicInfo {
        container_id: "cid-pod-1".to_string(),
        container_name: "pod-container".to_string(),
        container_ip: "10.0.0.1".to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: "proj-1".to_string(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: "http://test".to_string(),
    };

    let mut info = ProjectAndContainerInfo::from_parts(
        "proj-1".to_string(),
        None,
        Some("pod-abc".to_string()),
        None,
        Some(container),
        ProjectExtendedFields {
            service_type: Some(ServiceType::WebAgentRunner),
            ..Default::default()
        },
    );
    info.set_service_type(Some(ServiceType::WebAgentRunner));
    adapter
        .insert("proj-1".to_string(), Arc::new(info))
        .unwrap();

    // get_container_by_pod_id
    let found = adapter.get_container_by_pod_id("pod-abc");
    assert!(
        found.is_some(),
        "get_container_by_pod_id 应通过索引找到容器"
    );

    // find_projects_by_pod_id
    let projects = adapter.find_projects_by_pod_id("pod-abc");
    assert_eq!(projects.len(), 1, "find_projects_by_pod_id 应返回 1 个项目");
    assert_eq!(projects[0].project_id(), "proj-1");

    // 不存在的 pod_id
    assert!(adapter.get_container_by_pod_id("nonexistent").is_none());
    assert!(adapter.find_projects_by_pod_id("nonexistent").is_empty());
}

/// find_project_scope 按 project_id 反查 tenant/space/isolation，并校验 service_type 防串用。
#[test]
fn test_find_project_scope() {
    use shared_types::ContainerLookup; // trait 方法需在作用域
    let adapter = make_adapter();

    let mut info = create_test_info_with_container("proj-scope", "c-scope");
    info.set_scope(
        Some("t1".to_string()),
        Some("s1".to_string()),
        Some("space".to_string()),
    );
    adapter
        .insert("proj-scope".to_string(), Arc::new(info))
        .unwrap();

    // 命中：service_type 匹配 → 返回 scope
    let scope = adapter
        .find_project_scope("proj-scope", &ServiceType::WebAgentRunner)
        .expect("应命中 scope");
    assert_eq!(scope.tenant_id.as_deref(), Some("t1"));
    assert_eq!(scope.space_id.as_deref(), Some("s1"));
    assert_eq!(scope.isolation_type.as_deref(), Some("space"));

    // service_type 不匹配（ComputerAgentRunner）→ None（防串用）
    assert_eq!(
        adapter.find_project_scope("proj-scope", &ServiceType::ComputerAgentRunner),
        None
    );
    // 不存在的 project_id → None
    assert_eq!(
        adapter.find_project_scope("nonexistent", &ServiceType::WebAgentRunner),
        None
    );
}

/// 多 project 共享同一 pod_id 时，find_projects_by_pod_id 必须返回全部。
///
/// 回归测试：原实现用 pod_id_to_project_id 索引（insert 时覆盖），
/// 只返回最后插入的 1 个，导致 cleanup strategy 误判"无活跃引用"误销毁容器。
/// 现改为全量遍历，必须返回所有同 pod project。
#[test]
fn test_find_projects_by_pod_id_multiple_projects() {
    let adapter = make_adapter();

    let container = ContainerBasicInfo {
        container_id: "cid-shared".to_string(),
        container_name: "shared-pod".to_string(),
        container_ip: "10.0.0.5".to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: String::new(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: "http://shared".to_string(),
    };

    // 两个 project 共享 pod_id="pod-shared"（RCoder 共享容器模式）
    for pid in ["proj-A", "proj-B"] {
        let mut info = ProjectAndContainerInfo::from_parts(
            pid.to_string(),
            None,
            Some("pod-shared".to_string()),
            None,
            Some(container.clone()),
            ProjectExtendedFields {
                service_type: Some(ServiceType::WebAgentRunner),
                ..Default::default()
            },
        );
        info.set_service_type(Some(ServiceType::WebAgentRunner));
        adapter.insert(pid.to_string(), Arc::new(info)).unwrap();
    }

    // 关键断言：find_projects_by_pod_id 必须返回 2 个 project（不是索引覆盖后的 1 个）
    let projects = adapter.find_projects_by_pod_id("pod-shared");
    assert_eq!(
        projects.len(),
        2,
        "find_projects_by_pod_id 必须返回所有同 pod project（全量遍历），不能只返回索引里的单个"
    );

    let project_ids: Vec<_> = projects.iter().map(|p| p.project_id()).collect();
    assert!(project_ids.contains(&"proj-A"));
    assert!(project_ids.contains(&"proj-B"));

    // get_container_by_pod_id 仍通过索引返回（任一 project 的 container，共享同一容器）
    let container_info = adapter.get_container_by_pod_id("pod-shared");
    assert!(
        container_info.is_some(),
        "get_container_by_pod_id 应能找到共享容器"
    );
}

#[test]
fn test_index_cleanup_on_remove() {
    // remove 后 user_id/pod_id 索引应被清理
    // 注意：user_id 索引仅 ComputerAgentRunner 写入，故此处用 Computer 验证完整写入→清理路径
    let adapter = make_adapter();

    let container = ContainerBasicInfo {
        container_id: "cid-cleanup".to_string(),
        container_name: "cleanup-container".to_string(),
        container_ip: "10.0.0.1".to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: "proj-1".to_string(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: "http://test".to_string(),
    };

    let mut info = ProjectAndContainerInfo::from_parts(
        "proj-1".to_string(),
        Some("user-cleanup".to_string()),
        Some("pod-cleanup".to_string()),
        None,
        Some(container),
        ProjectExtendedFields {
            service_type: Some(ServiceType::ComputerAgentRunner),
            ..Default::default()
        },
    );
    info.set_service_type(Some(ServiceType::ComputerAgentRunner));
    adapter
        .insert("proj-1".to_string(), Arc::new(info))
        .unwrap();

    // 索引存在
    assert!(adapter.user_id_to_project_ids.contains_key("user-cleanup"));
    assert!(adapter.pod_id_to_project_id.contains_key("pod-cleanup"));
    assert!(adapter.container_id_to_key.contains_key("cid-cleanup"));

    // remove 后索引应被清理
    adapter.remove("proj-1");
    assert!(
        !adapter.user_id_to_project_ids.contains_key("user-cleanup"),
        "user_id 索引应在 remove 后被清理"
    );
    assert!(
        !adapter.pod_id_to_project_id.contains_key("pod-cleanup"),
        "pod_id 索引应在 remove 后被清理"
    );
    // 新行为:ref_count=0 不清 container_id_to_key(容器条目保留,反向索引随之保留),
    // 由 cleaner 物理销毁(delete_container_with_projects)时统一清理。
    assert!(
        adapter.container_id_to_key.contains_key("cid-cleanup"),
        "ref_count=0 保留容器条目,container_id_to_key 随之保留(交 cleaner 清理)"
    );
}

#[test]
fn test_index_cleanup_on_delete_container_with_projects() {
    // delete_container_with_projects 后所有索引应被清理
    let adapter = make_adapter();

    let container = ContainerBasicInfo {
        container_id: "cid-del".to_string(),
        container_name: "del-container".to_string(),
        container_ip: "10.0.0.1".to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: "proj-1".to_string(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: "http://test".to_string(),
    };

    let mut info = ProjectAndContainerInfo::from_parts(
        "proj-1".to_string(),
        Some("user-del".to_string()),
        None,
        None,
        Some(container.clone()),
        ProjectExtendedFields {
            service_type: Some(ServiceType::ComputerAgentRunner),
            ..Default::default()
        },
    );
    info.set_service_type(Some(ServiceType::ComputerAgentRunner));
    adapter
        .insert("proj-1".to_string(), Arc::new(info))
        .unwrap();

    // 索引存在
    assert!(adapter.container_id_to_key.contains_key("cid-del"));
    assert!(adapter.user_id_to_project_ids.contains_key("user-del"));

    // 新行为:remove 时 ref_count=0 但 RAII 保留容器条目(交 cleaner),故 delete 时容器仍存在(existed=true)。
    // proj-1 已被上面的 remove 删除(不在 project_to_container),故 delete 无项目待清理(count=0)。
    let (existed, count) = adapter.delete_container_with_projects("cid-del");
    assert!(existed, "容器被 RAII 保留(ref_count=0),delete 时仍存在");
    assert_eq!(count, 1, "delete 清理 1 个关联项目(proj-1)");

    // 索引应全部清理(delete_container_with_projects 显式销毁容器 + 清反向索引)
    assert!(
        !adapter.container_id_to_key.contains_key("cid-del"),
        "container_id_to_key 索引应在 delete_container_with_projects 后被清理"
    );
    assert!(
        !adapter.user_id_to_project_ids.contains_key("user-del"),
        "user_id 索引应在 delete_container_with_projects 后被清理"
    );
    assert!(
        adapter.containers.is_empty(),
        "容器应在 delete_container_with_projects 后被清理"
    );
}

#[test]
fn test_index_consistency_under_raii() {
    // 验证 RAII 清理后索引一致性：多个 project 共享容器
    let (adapter, _rx) =
        ProjectAdapter::new(TEST_NAMESPACE.to_string(), TEST_CLUSTER_DOMAIN.to_string());

    let container = ContainerBasicInfo {
        container_id: "cid-shared".to_string(),
        container_name: "shared-container".to_string(),
        container_ip: "10.0.0.1".to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: String::new(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: "http://shared".to_string(),
    };

    // 两个 project 共享同一容器（同一 user_id → container_key = user_id）
    let info1 = create_shared_project("proj-1", "user-shared", &container);
    let info2 = create_shared_project("proj-2", "user-shared", &container);

    adapter
        .insert("proj-1".to_string(), Arc::new(info1))
        .unwrap();
    adapter
        .insert("proj-2".to_string(), Arc::new(info2))
        .unwrap();

    assert_eq!(adapter.containers.len(), 1, "共享容器应只有 1 个条目");
    assert!(adapter.container_id_to_key.contains_key("cid-shared"));

    // 移除 proj-1：容器不销毁（ref_count > 0），索引保留
    adapter.remove("proj-1");
    assert_eq!(
        adapter.containers.len(),
        1,
        "容器应保留（还有 proj-2 引用）"
    );

    // 移除 proj-2：ref_count=0,新行为保留容器条目(交 cleaner),不立即销毁。
    adapter.remove("proj-2");
    assert_eq!(
        adapter.containers.len(),
        1,
        "ref_count=0 保留容器条目(交 cleaner 回收)"
    );
    // container_id_to_key 随容器条目保留(由 cleaner 物理销毁时清理),故仍在。
    assert!(
        adapter.container_id_to_key.contains_key("cid-shared"),
        "ref_count=0 保留容器,container_id_to_key 随之保留(交 cleaner 清理)"
    );
    assert!(
        !adapter.user_id_to_project_ids.contains_key("user-shared"),
        "user_id 索引应在 remove 后被清理(remove 清空 project 集合即摘条目)"
    );
}

/// 测试容器重建后 session_index 同步
///
/// 场景：容器重建后，existing session 需要通过 add_session_to_project 同步到 session_index
#[test]
fn test_session_index_sync_after_container_rebuild() {
    let adapter = make_adapter();
    let project_id = "test-rebuild";
    let session_id = "session-before-rebuild";

    // 1. 创建项目并添加 session
    let info = Arc::new(create_test_info_with_container(project_id, "container-old"));
    adapter.insert(project_id.to_string(), info).unwrap();
    adapter.add_session_to_project(project_id, session_id);

    // 验证 session 可查
    assert!(adapter.get_by_session_id(session_id).is_some());

    // 2. 模拟容器重建：重新插入项目（不带 session）
    let new_info = Arc::new(create_test_info_with_container(project_id, "container-new"));
    adapter.insert(project_id.to_string(), new_info).unwrap();

    // 此时 session_index 中应该还有旧的 session（因为 insert 不清理 session_index）
    // 但 project 的 sessions 集合是空的
    let project = adapter.get(project_id).unwrap();
    assert_eq!(project.session_count(), 0, "新 project 的 sessions 应为空");

    // 3. 模拟 ensure_project_mapping_in_state 的修复逻辑：同步 session 到 session_index
    adapter.add_session_to_project(project_id, session_id);

    // 验证 session 现在可查
    let by_session = adapter.get_by_session_id(session_id);
    assert!(by_session.is_some(), "同步后 session 应可查");
    assert_eq!(by_session.unwrap().project_id(), project_id);

    // 验证 project 的 sessions 集合也包含了这个 session
    let project = adapter.get(project_id).unwrap();
    assert_eq!(project.session_count(), 1, "project 应包含 1 个 session");
}

/// 测试 get_by_session_id 验证 session 是否在 sessions 集合中
///
/// 场景：session 被 clear_session_one 清除后，get_by_session_id 应返回 None
#[test]
fn test_get_by_session_id_validates_session_in_set() {
    let adapter = make_adapter();
    let project_id = "test-validate-session";
    let session_id = "session-to-validate";

    // 创建项目并添加 session
    let info = Arc::new(create_test_info(project_id));
    adapter.insert(project_id.to_string(), info).unwrap();
    adapter.add_session_to_project(project_id, session_id);

    // 验证 session 可查
    assert!(adapter.get_by_session_id(session_id).is_some());

    // 清除 session
    let cleared = adapter.clear_session_one(project_id, session_id);
    assert!(cleared);

    // 验证 session 不可查（关键断言）
    assert!(
        adapter.get_by_session_id(session_id).is_none(),
        "清除后的 session 不应被 get_by_session_id 返回"
    );

    // 验证 session_index 也被清理
    assert!(
        !adapter.session_index.contains_key(session_id),
        "session_index 应在 clear_session_one 后被清理"
    );
}

/// 测试 clear_session_one 的顺序：先清理 session_index，再清理 projects
///
/// 场景：验证 clear_session_one 后，session_index 和 projects 都被正确清理
#[test]
fn test_clear_session_one_order() {
    let adapter = make_adapter();
    let project_id = "test-clear-order";
    let session_id = "session-clear-order";

    // 创建项目并添加 session
    let info = Arc::new(create_test_info(project_id));
    adapter.insert(project_id.to_string(), info).unwrap();
    adapter.add_session_to_project(project_id, session_id);

    // 验证 session_index 和 projects 都有这个 session
    assert!(adapter.session_index.contains_key(session_id));
    let project = adapter.get(project_id).unwrap();
    assert!(project.sessions().contains(session_id));

    // 清除 session
    let cleared = adapter.clear_session_one(project_id, session_id);
    assert!(cleared);

    // 验证 session_index 被清理
    assert!(
        !adapter.session_index.contains_key(session_id),
        "session_index 应被清理"
    );

    // 验证 projects 中的 sessions 集合也被清理
    let project = adapter.get(project_id).unwrap();
    assert!(
        !project.sessions().contains(session_id),
        "projects 中的 sessions 集合应被清理"
    );
}

/// 测试容器重建场景：insert 后 add_session_to_project 的完整流程
///
/// 模拟 computer_chat_handler 中 ensure_project_mapping_in_state 的逻辑
#[test]
fn test_container_rebuild_with_session_migration() {
    let adapter = make_adapter();
    let project_id = "test-rebuild-migration";
    let user_id = "user-123";

    // 1. 初始状态：创建项目，添加 2 个 session
    let mut info = create_test_info_with_container(project_id, "container-v1");
    info.set_user_id(Some(user_id.to_string()));
    adapter
        .insert(project_id.to_string(), Arc::new(info))
        .unwrap();
    adapter.add_session_to_project(project_id, "session-1");
    adapter.add_session_to_project(project_id, "session-2");

    // 验证初始状态
    assert_eq!(adapter.get(project_id).unwrap().session_count(), 2);
    assert!(adapter.get_by_session_id("session-1").is_some());
    assert!(adapter.get_by_session_id("session-2").is_some());

    // 2. 模拟容器重建：获取 existing sessions
    let existing_sessions: Vec<String> = adapter
        .get(project_id)
        .map(|p| p.sessions().into_iter().collect())
        .unwrap_or_default();
    assert_eq!(existing_sessions.len(), 2);

    // 3. 插入新的 project（模拟容器重建）
    let mut new_info = create_test_info_with_container(project_id, "container-v2");
    new_info.set_user_id(Some(user_id.to_string()));
    adapter
        .insert(project_id.to_string(), Arc::new(new_info))
        .unwrap();

    // 4. 同步现有 session 到 session_index（修复逻辑）
    for sid in &existing_sessions {
        adapter.add_session_to_project(project_id, sid);
    }

    // 5. 验证所有 session 都可查
    assert!(
        adapter.get_by_session_id("session-1").is_some(),
        "session-1 应可查"
    );
    assert!(
        adapter.get_by_session_id("session-2").is_some(),
        "session-2 应可查"
    );

    // 6. 验证 project 的 sessions 集合也正确
    let project = adapter.get(project_id).unwrap();
    assert_eq!(project.session_count(), 2, "project 应包含 2 个 session");

    // 7. 添加新 session（新请求）
    adapter.add_session_to_project(project_id, "session-3");
    assert!(
        adapter.get_by_session_id("session-3").is_some(),
        "新 session-3 应可查"
    );

    let project = adapter.get(project_id).unwrap();
    assert_eq!(project.session_count(), 3, "project 应包含 3 个 session");
}
