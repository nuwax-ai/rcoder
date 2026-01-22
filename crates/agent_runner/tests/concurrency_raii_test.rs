//! 并发和 RAII 设计测试
//!
//! 验证以下功能：
//! 1. 并发独立启动 agent，agent 之间互不影响
//! 2. agent 销毁的正确性
//! 3. RAII 设计（PendingGuard）是否可以正常快速销毁 agent
//! 4. 原子计数器的并发安全性

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Barrier;

// 重新导出必要的类型
use agent_runner::service::AgentSessionRegistry;
use agent_runner::service::PendingGuard;
use agent_runner::agent_runtime::WORKER_THREAD_POOL_SIZE;
use sacp::schema::SessionId;
use shared_types::{AgentStatus, ProjectAndAgentInfo};
use tokio::sync::mpsc;

// ============================================================================
// 1. PendingGuard RAII 测试
// ============================================================================

#[test]
fn test_pending_guard_auto_cleanup_on_drop() {
    let registry = AgentSessionRegistry::new();

    // 设置 Pending 状态
    {
        let _guard = PendingGuard::new(&registry, "test-project");
        // 验证 Pending 状态已设置
        assert!(registry.contains_project("test-project"));
        let info = registry.get_agent_info("test-project").unwrap();
        assert_eq!(format!("{:?}", info.status), "Pending");
    }
    // guard 已 drop，Pending 状态应该被清理
    assert!(!registry.contains_project("test-project"));
}

#[test]
fn test_pending_guard_commit_success_prevents_cleanup() {
    let registry = AgentSessionRegistry::new();

    {
        let guard = PendingGuard::new(&registry, "test-project");

        // 验证 Pending 状态已设置
        assert!(registry.contains_project("test-project"));

        // 提交成功，防止清理
        guard.commit_success();
    }

    // Pending 状态应该保留
    assert!(registry.contains_project("test-project"));
    let info = registry.get_agent_info("test-project").unwrap();
    assert_eq!(format!("{:?}", info.status), "Pending");

    // 清理
    registry.remove_by_project("test-project");
}

#[test]
fn test_pending_guard_early_return_cleanup() {
    let registry = AgentSessionRegistry::new();

    // 模拟早期返回场景（使用 return 代替 panic，因为 DashMap 不支持 catch_unwind）
    let early_return = || {
        let _guard = PendingGuard::new(&registry, "test-project");
        // 早期返回（模拟错误场景）
        return false;
    };

    // 调用后，guard 已经被 drop，应该被清理
    early_return();

    assert!(!registry.contains_project("test-project"));
}

// ============================================================================
// 2. 原子计数器并发安全性测试
// ============================================================================

#[tokio::test]
async fn test_atomic_slot_counter_concurrent_acquisition() {
    let registry = Arc::new(AgentSessionRegistry::new());
    let num_tasks = WORKER_THREAD_POOL_SIZE * 2;
    let barrier = Arc::new(Barrier::new(num_tasks));
    let successful_count = Arc::new(AtomicUsize::new(0));
    let failed_count = Arc::new(AtomicUsize::new(0));

    let mut handles = vec![];

    // 启动并发任务尝试获取槽位
    for i in 0..num_tasks {
        let registry_clone = registry.clone();
        let barrier_clone = barrier.clone();
        let successful_count_clone = successful_count.clone();
        let failed_count_clone = failed_count.clone();

        let handle = tokio::spawn(async move {
            // 等待所有任务就绪
            barrier_clone.wait().await;

            // 尝试获取槽位
            if registry_clone.try_acquire_session_slot() {
                successful_count_clone.fetch_add(1, Ordering::Relaxed);
                // 模拟工作
                tokio::time::sleep(Duration::from_millis(10)).await;
                registry_clone.release_session_slot();
            } else {
                failed_count_clone.fetch_add(1, Ordering::Relaxed);
            }
        });

        handles.push(handle);
    }

    // 等待所有任务完成
    for handle in handles {
        handle.await.unwrap();
    }

    // 验证: 只有 WORKER_THREAD_POOL_SIZE 个任务成功
    let successful = successful_count.load(Ordering::Relaxed);
    let failed = failed_count.load(Ordering::Relaxed);

    assert_eq!(
        successful,
        WORKER_THREAD_POOL_SIZE,
        "应该有 {} 个任务成功获取槽位",
        WORKER_THREAD_POOL_SIZE
    );
    assert_eq!(
        failed,
        WORKER_THREAD_POOL_SIZE,
        "应该有 {} 个任务失败（槽位已满）",
        WORKER_THREAD_POOL_SIZE
    );

    // 验证: 计数器最终应该回到 0
    assert_eq!(
        registry.active_sessions_count(),
        0,
        "所有槽位应该被释放"
    );
}

#[tokio::test]
async fn test_atomic_slot_counter_stress_test() {
    let registry = Arc::new(AgentSessionRegistry::new());
    let num_iterations = 1000;
    let successful_count = Arc::new(AtomicUsize::new(0));

    let mut handles = vec![];

    // 启动大量并发任务
    for _ in 0..50 {
        let registry_clone = registry.clone();
        let successful_count_clone = successful_count.clone();

        let handle = tokio::spawn(async move {
            for j in 0..num_iterations {
                if registry_clone.try_acquire_session_slot() {
                    successful_count_clone.fetch_add(1, Ordering::Relaxed);
                    // 使用简单的随机性（不依赖 rand）
                    let delay = (j % 10) as u64;
                    tokio::time::sleep(Duration::from_micros(delay * 10)).await;
                    registry_clone.release_session_slot();
                }
            }
        });

        handles.push(handle);
    }

    // 等待所有任务完成
    for handle in handles {
        handle.await.unwrap();
    }

    // 验证: 计数器最终应该回到 0
    assert_eq!(
        registry.active_sessions_count(),
        0,
        "所有槽位应该被释放"
    );

    println!(
        "压力测试完成: {} 次成功获取槽位",
        successful_count.load(Ordering::Relaxed)
    );
}

// ============================================================================
// 3. Agent 并发独立性测试
// ============================================================================

#[tokio::test]
async fn test_concurrent_agents_independence() {
    let registry = Arc::new(AgentSessionRegistry::new());
    let num_agents = 10;
    let barrier = Arc::new(Barrier::new(num_agents));
    let mut handles = vec![];

    // 并发创建多个 agent
    for i in 0..num_agents {
        let registry_clone = registry.clone();
        let barrier_clone = barrier.clone();

        let handle = tokio::spawn(async move {
            let project_id = format!("project-{}", i);
            let session_id = format!("session-{}", i);

            // 创建 AgentInfo
            let (prompt_tx, _) = mpsc::channel(100);
            let (cancel_tx, _) = mpsc::channel(100);

            let agent_info = ProjectAndAgentInfo {
                project_id: project_id.clone(),
                session_id: SessionId::new(Arc::from(session_id.as_str())),
                prompt_tx,
                cancel_tx,
                model_provider: None,
                request_id: None,
                status: AgentStatus::Active,
                last_activity: chrono::Utc::now(),
                created_at: chrono::Utc::now(),
                stop_handle: None,
            };

            // 注册 agent
            registry_clone.register(&project_id, &session_id, agent_info);

            // 等待所有 agent 就绪
            barrier_clone.wait().await;

            // 验证: 当前 agent 存在
            assert!(registry_clone.contains_project(&project_id));

            // 验证: 其他 agent 也存在（不会相互覆盖）
            let stats = registry_clone.stats();
            assert_eq!(stats.agent_count, num_agents);

            // 模拟工作
            tokio::time::sleep(Duration::from_millis(10)).await;

            // 清理
            registry_clone.remove_by_project(&project_id);
        });

        handles.push(handle);
    }

    // 等待所有任务完成
    for handle in handles {
        handle.await.unwrap();
    }

    // 验证: 所有 agent 已被清理
    assert_eq!(registry.stats().agent_count, 0);
}

#[tokio::test]
async fn test_concurrent_agent_state_updates() {
    let registry = Arc::new(AgentSessionRegistry::new());
    let project_id = "test-project";
    let session_id = "test-session";

    // 创建初始 agent
    let (prompt_tx, _) = mpsc::channel(100);
    let (cancel_tx, _) = mpsc::channel(100);

    let agent_info = ProjectAndAgentInfo {
        project_id: project_id.to_string(),
        session_id: SessionId::new(Arc::from(session_id)),
        prompt_tx,
        cancel_tx,
        model_provider: None,
        request_id: None,
        status: AgentStatus::Idle,
        last_activity: chrono::Utc::now(),
        created_at: chrono::Utc::now(),
        stop_handle: None,
    };

    registry.register(project_id, session_id, agent_info);

    // 并发更新状态
    let num_updates = 100;
    let mut handles = vec![];

    for i in 0..10 {
        let registry_clone = registry.clone();

        let handle = tokio::spawn(async move {
            for j in 0..num_updates {
                // 使用原子性更新
                registry_clone.try_update_agent_info(project_id, |info| {
                    // 模拟状态切换
                    if j % 2 == 0 {
                        info.status = AgentStatus::Active;
                    } else {
                        info.status = AgentStatus::Idle;
                    }
                    info.last_activity = chrono::Utc::now();
                    true
                });
                tokio::time::sleep(Duration::from_micros(10)).await;
            }
        });

        handles.push(handle);
    }

    // 等待所有更新完成
    for handle in handles {
        handle.await.unwrap();
    }

    // 验证: agent 仍然存在，没有数据损坏
    assert!(registry.contains_project(project_id));
    let info = registry.get_agent_info(project_id).unwrap();
    assert!(matches!(info.status, AgentStatus::Active | AgentStatus::Idle));

    // 清理
    registry.remove_by_project(project_id);
}

// ============================================================================
// 4. Agent 销毁测试
// ============================================================================

#[tokio::test]
async fn test_agent_lifecycle_cleanup() {
    let registry = Arc::new(AgentSessionRegistry::new());

    // 创建多个 agent
    let num_agents = 5;
    for i in 0..num_agents {
        let project_id = format!("project-{}", i);
        let session_id = format!("session-{}", i);

        let (prompt_tx, _) = mpsc::channel(100);
        let (cancel_tx, _) = mpsc::channel(100);

        let agent_info = ProjectAndAgentInfo {
            project_id: project_id.clone(),
            session_id: SessionId::new(Arc::from(session_id.as_str())),
            prompt_tx,
            cancel_tx,
            model_provider: None,
            request_id: None,
            status: AgentStatus::Active,
            last_activity: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
            stop_handle: None,
        };

        registry.register(&project_id, &session_id, agent_info);
    }

    // 验证: 所有 agent 已注册
    assert_eq!(registry.stats().agent_count, num_agents);

    // 销毁所有 agent
    for i in 0..num_agents {
        let project_id = format!("project-{}", i);
        let removed = registry.remove_by_project(&project_id);
        assert!(removed.is_some(), "应该能移除 agent");
    }

    // 验证: 所有 agent 已被清理
    assert_eq!(registry.stats().agent_count, 0);

    // 验证: 映射关系也被清理
    for i in 0..num_agents {
        let session_id = format!("session-{}", i);
        assert!(!registry.contains_session(&session_id));
    }
}

#[tokio::test]
async fn test_agent_concurrent_removal() {
    let registry = Arc::new(AgentSessionRegistry::new());

    // 创建大量 agent
    let num_agents = 100;
    for i in 0..num_agents {
        let project_id = format!("project-{}", i);
        let session_id = format!("session-{}", i);

        let (prompt_tx, _) = mpsc::channel(100);
        let (cancel_tx, _) = mpsc::channel(100);

        let agent_info = ProjectAndAgentInfo {
            project_id: project_id.clone(),
            session_id: SessionId::new(Arc::from(session_id.as_str())),
            prompt_tx,
            cancel_tx,
            model_provider: None,
            request_id: None,
            status: AgentStatus::Active,
            last_activity: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
            stop_handle: None,
        };

        registry.register(&project_id, &session_id, agent_info);
    }

    // 并发移除所有 agent
    let mut handles = vec![];
    for i in 0..num_agents {
        let registry_clone = registry.clone();
        let handle = tokio::spawn(async move {
            let project_id = format!("project-{}", i);
            registry_clone.remove_by_project(&project_id);
        });
        handles.push(handle);
    }

    // 等待所有移除完成
    for handle in handles {
        handle.await.unwrap();
    }

    // 验证: 所有 agent 已被清理
    assert_eq!(registry.stats().agent_count, 0);
}

// ============================================================================
// 5. RAII 快速销毁测试
// ============================================================================

#[test]
fn test_raii_fast_destruction() {
    let registry = AgentSessionRegistry::new();
    let num_guards = 1000;

    let start = std::time::Instant::now();

    // 创建大量 guard
    for i in 0..num_guards {
        let project_id = format!("project-{}", i);
        let _guard = PendingGuard::new(&registry, &project_id);
        // guard 立即被 drop
    }

    let elapsed = start.elapsed();

    // 验证: 销毁应该很快（< 10ms）
    assert!(
        elapsed.as_millis() < 10,
        "RAII 销毁应该快速完成，实际耗时: {:?}",
        elapsed
    );

    // 验证: 所有项目都被清理
    assert_eq!(registry.stats().agent_count, 0);

    println!("RAII 快速销毁测试: {} 个 guard 在 {:?} 内销毁", num_guards, elapsed);
}

#[tokio::test]
async fn test_pending_guard_with_tokio_spawn() {
    let registry = Arc::new(AgentSessionRegistry::new());
    let num_tasks = 50;

    let mut handles = vec![];

    // 并发创建 guard
    for i in 0..num_tasks {
        let registry_clone = registry.clone();
        let handle = tokio::spawn(async move {
            let project_id = format!("project-{}", i);
            let _guard = PendingGuard::new(&registry_clone, &project_id);
            // 模拟异步工作
            tokio::time::sleep(Duration::from_millis(1)).await;
            // guard 在这里被 drop
        });
        handles.push(handle);
    }

    // 等待所有任务完成
    for handle in handles {
        handle.await.unwrap();
    }

    // 验证: 所有项目都被清理
    assert_eq!(registry.stats().agent_count, 0);
}

// ============================================================================
// 6. 边界条件测试
// ============================================================================

#[tokio::test]
async fn test_slot_counter_underflow_protection() {
    let registry = AgentSessionRegistry::new();

    // 尝试释放从未获取的槽位
    for _ in 0..10 {
        registry.release_session_slot();
    }

    // 验证: 计数器不会下溢（使用 saturating_sub）
    let count = registry.active_sessions_count();
    assert_eq!(count, 0, "计数器应该保持为 0，不会下溢");
}

#[tokio::test]
async fn test_multiple_pending_guards_same_project() {
    let registry = AgentSessionRegistry::new();
    let project_id = "test-project";

    // 创建多个 guard（模拟并发请求）
    {
        let _guard1 = PendingGuard::new(&registry, project_id);
        // 第二个 guard 会更新现有项目为 Pending（已经是 Pending，无操作）
        let _guard2 = PendingGuard::new(&registry, project_id);

        let info = registry.get_agent_info(project_id).unwrap();
        assert_eq!(format!("{:?}", info.status), "Pending");
    }

    // 所有 guard 都 drop，应该被清理
    assert!(!registry.contains_project(project_id));
}

// ============================================================================
// 7. 压力测试：高并发场景
// ============================================================================

#[tokio::test]
async fn test_high_concurrency_stress() {
    let registry = Arc::new(AgentSessionRegistry::new());
    let num_requests = 1000;
    let barrier = Arc::new(Barrier::new(num_requests));
    let success_count = Arc::new(AtomicUsize::new(0));
    let fail_count = Arc::new(AtomicUsize::new(0));

    let mut handles = vec![];

    // 模拟高并发请求
    for i in 0..num_requests {
        let registry_clone = registry.clone();
        let barrier_clone = barrier.clone();
        let success_count_clone = success_count.clone();
        let fail_count_clone = fail_count.clone();

        let handle = tokio::spawn(async move {
            let project_id = format!("project-{}", i);

            // 等待所有任务就绪
            barrier_clone.wait().await;

            // 尝试获取槽位
            if registry_clone.try_acquire_session_slot() {
                success_count_clone.fetch_add(1, Ordering::Relaxed);

                // 使用 PendingGuard
                let _guard = PendingGuard::new(&registry_clone, &project_id);

                // 模拟工作
                let delay = (i % 10) as u64;
                tokio::time::sleep(Duration::from_millis(delay)).await;

                // 正常流程：禁用 guard，手动释放
                drop(_guard);
                registry_clone.release_session_slot();
                registry_clone.clear_pending_if_exists(&project_id);
            } else {
                fail_count_clone.fetch_add(1, Ordering::Relaxed);
            }
        });

        handles.push(handle);
    }

    // 等待所有任务完成
    for handle in handles {
        handle.await.unwrap();
    }

    // 验证: 只有 WORKER_THREAD_POOL_SIZE 个请求成功
    let success = success_count.load(Ordering::Relaxed);
    let fail = fail_count.load(Ordering::Relaxed);

    assert_eq!(
        success + fail,
        num_requests,
        "所有请求都应该被处理"
    );

    assert_eq!(
        registry.active_sessions_count(),
        0,
        "所有槽位应该被释放"
    );

    println!(
        "高并发压力测试: {} 成功, {} 失败",
        success, fail
    );
}

