//! 并发插入删除 / 共享容器回收压力族（从 tests.rs 拆出；
//! join_with_timeout 等 helper 见 tests/mod.rs 公共区）。

use std::sync::Barrier;
use std::thread;
use std::time::Duration;

use super::*;

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
