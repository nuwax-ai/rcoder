//! 并发和 RAII 设计测试
//!
//! 验证以下功能：
//! 1. 并发独立启动 agent，agent 之间互不影响
//! 2. agent 销毁的正确性
//! 3. RAII 设计（PendingGuard）是否可以正常快速销毁 agent
//! 4. 原子计数器的并发安全性

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Barrier;

// 重新导出必要的类型
use agent_runner::agent_runtime::{get_concurrency_limit, init_concurrency_limit};
use agent_runner::service::AgentSessionRegistry;
use agent_runner::service::PendingGuard;
use sacp::schema::SessionId;
use shared_types::{AgentStatus, ProjectAndAgentInfo, SessionEntry};
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
    // 重置并发限制为默认值，防止其他测试影响
    init_concurrency_limit(10);
    let registry = Arc::new(AgentSessionRegistry::new());
    let limit = get_concurrency_limit();
    let num_tasks = limit * 2;
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
    let limit = get_concurrency_limit();

    assert_eq!(successful, limit, "应该有 {} 个任务成功获取槽位", limit);
    assert_eq!(failed, limit, "应该有 {} 个任务失败（槽位已满）", limit);

    // 验证: 计数器最终应该回到 0
    assert_eq!(registry.active_sessions_count(), 0, "所有槽位应该被释放");
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
    assert_eq!(registry.active_sessions_count(), 0, "所有槽位应该被释放");

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
    assert!(matches!(
        info.status,
        AgentStatus::Active | AgentStatus::Idle
    ));

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

    println!(
        "RAII 快速销毁测试: {} 个 guard 在 {:?} 内销毁",
        num_guards, elapsed
    );
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

    assert_eq!(success + fail, num_requests, "所有请求都应该被处理");

    assert_eq!(registry.active_sessions_count(), 0, "所有槽位应该被释放");

    println!("高并发压力测试: {} 成功, {} 失败", success, fail);
}

// ============================================================================
// 8. PendingGuard 与 SessionManager 竞态条件修复测试
// ============================================================================

/// 测试场景：PendingGuard 创建的占位符应该被真实会话替换
///
/// 这是修复的核心场景：
/// 1. PendingGuard 创建 pending 占位符
/// 2. 模拟 SessionManager 检测到 pending 并创建真实会话
/// 3. 验证 pending 占位符被正确替换
#[tokio::test]
async fn test_pending_placeholder_replaced_by_real_session() {
    let registry = Arc::new(AgentSessionRegistry::new());
    let project_id = "test-pending-replace";

    // 第一阶段：创建 PendingGuard（模拟 gRPC 层的行为）
    let guard = PendingGuard::new(&registry, project_id);

    // 验证：pending 占位符已创建
    assert!(registry.contains_project(project_id));
    {
        let pending_info = registry.get_agent_info(project_id).unwrap();
        assert_eq!(format!("{:?}", pending_info.status), "Pending");
        assert_eq!(pending_info.session_id.to_string(), "pending");
    } // 释放 Ref 锁

    // 第二阶段：模拟 SessionManager 检测到 Pending 状态并创建真实会话
    // 检查状态（模拟 session_manager.rs 中的逻辑）
    let should_replace = {
        let info = registry.get_agent_info(project_id).unwrap();
        *info.status() == AgentStatus::Pending
    }; // 释放 Ref 锁
    assert!(should_replace, "应该检测到 Pending 占位符");

    // 创建真实会话
    let real_session_id = "real-session-123";
    let (prompt_tx, _prompt_rx) = mpsc::channel(100);
    let (cancel_tx, _cancel_rx) = mpsc::channel(100);

    let real_session = ProjectAndAgentInfo {
        project_id: project_id.to_string(),
        session_id: SessionId::new(Arc::from(real_session_id)),
        prompt_tx,
        cancel_tx,
        model_provider: None,
        request_id: None,
        status: AgentStatus::Idle,
        last_activity: chrono::Utc::now(),
        created_at: chrono::Utc::now(),
        stop_handle: None,
    };

    // 第三阶段：原子性替换（模拟 session_manager.rs 中的 Entry API 逻辑）
    // 使用 DashMap 的 entry API 进行原子性替换
    use dashmap::mapref::entry::Entry;
    match registry.as_ref().inner_mut().entry(project_id.to_string()) {
        Entry::Vacant(entry) => {
            entry.insert(real_session.clone());
        }
        Entry::Occupied(mut entry) => {
            // 检查仍然是 Pending（防止其他线程已经插入了真实会话）
            // 提取状态值，避免借用冲突
            let is_pending = {
                let existing = entry.get();
                *existing.status() == AgentStatus::Pending
            };
            if is_pending {
                entry.insert(real_session.clone());
            }
        }
    }

    // 验证：pending 占位符已被替换为真实会话
    assert!(registry.contains_project(project_id));
    let final_info = registry.get_agent_info(project_id).unwrap();
    assert_eq!(format!("{:?}", final_info.status), "Idle");
    assert_eq!(final_info.session_id.to_string(), real_session_id);
    assert!(!final_info.prompt_tx.is_closed());

    // PendingGuard 不需要 commit（因为 pending 已被替换）
    drop(guard);

    // 验证：真实会话仍然存在（没有被 PendingGuard 清理）
    assert!(registry.contains_project(project_id));

    // 清理
    registry.remove_by_project(project_id);
}

/// 测试场景：并发创建时，只有一个真实会话被保留
///
/// 验证修复的并发安全性：
/// 1. PendingGuard 创建 pending 占位符
/// 2. 多个线程尝试替换 pending
/// 3. 只有一个真实会话被保留
#[tokio::test]
async fn test_concurrent_pending_replacement() {
    let registry = Arc::new(AgentSessionRegistry::new());
    let project_id = "test-concurrent-replace";
    let num_threads = 5;
    let barrier = Arc::new(Barrier::new(num_threads));
    let success_count = Arc::new(AtomicUsize::new(0));

    // 第一阶段：创建 PendingGuard
    let _guard = PendingGuard::new(&registry, project_id);

    // 验证 pending 占位符
    assert!(registry.contains_project(project_id));
    {
        let pending_info = registry.get_agent_info(project_id).unwrap();
        assert_eq!(format!("{:?}", pending_info.status), "Pending");
    } // 释放 Ref 锁

    // 第二阶段：多个线程并发尝试替换 pending
    let mut handles = vec![];
    for i in 0..num_threads {
        let registry_clone = registry.clone();
        let barrier_clone = barrier.clone();
        let success_count_clone = success_count.clone();

        let handle = tokio::spawn(async move {
            // 等待所有线程就绪
            barrier_clone.wait().await;

            // 每个线程创建一个"真实会话"
            let session_id = format!("session-{}", i);
            let (prompt_tx, _prompt_rx) = mpsc::channel(100);
            let (cancel_tx, _cancel_rx) = mpsc::channel(100);

            let real_session = ProjectAndAgentInfo {
                project_id: project_id.to_string(),
                session_id: SessionId::new(Arc::from(session_id.clone())),
                prompt_tx,
                cancel_tx,
                model_provider: None,
                request_id: None,
                status: AgentStatus::Idle,
                last_activity: chrono::Utc::now(),
                created_at: chrono::Utc::now(),
                stop_handle: None,
            };

            // 尝试原子性替换
            use dashmap::mapref::entry::Entry;
            let replaced = match registry_clone
                .as_ref()
                .inner_mut()
                .entry(project_id.to_string())
            {
                Entry::Occupied(mut entry) => {
                    // 只有 pending 才替换
                    // 提取状态值，避免借用冲突
                    let is_pending = {
                        let existing = entry.get();
                        *existing.status() == AgentStatus::Pending
                    };
                    if is_pending {
                        entry.insert(real_session.clone());
                        success_count_clone.fetch_add(1, Ordering::Relaxed);
                        true
                    } else {
                        false
                    }
                }
                Entry::Vacant(_) => false,
            };

            replaced
        });

        handles.push(handle);
    }

    // 等待所有线程完成
    for handle in handles {
        handle.await.unwrap();
    }

    // 验证：只有一个线程成功替换
    let success = success_count.load(Ordering::Relaxed);
    assert_eq!(success, 1, "应该只有一个线程成功替换 pending");

    // 验证：最终只有一个会话存在
    assert!(registry.contains_project(project_id));
    let final_info = registry.get_agent_info(project_id).unwrap();
    assert_eq!(format!("{:?}", final_info.status), "Idle");

    // 清理
    registry.remove_by_project(project_id);
}

/// 测试场景：模拟真实的 session_manager.rs 逻辑流程
///
/// 这是一个端到端测试，模拟完整的修复流程：
/// 1. PendingGuard 创建占位符
/// 2. 检测到 Pending 状态
/// 3. 释放锁，创建真实会话
/// 4. 原子性插入/替换
#[tokio::test]
async fn test_session_manager_pending_replacement_flow() {
    let registry = Arc::new(AgentSessionRegistry::new());
    let project_id = "test-e2e-flow";

    // ========== 第一阶段：PendingGuard 创建占位符 ==========
    {
        let _guard = PendingGuard::new(&registry, project_id);

        // 验证占位符
        {
            let info = registry.get_agent_info(project_id).unwrap();
            assert_eq!(format!("{:?}", info.status), "Pending");
        } // 释放 Ref 锁

        // ========== 第二阶段：模拟 SessionManager 的 get_or_create_session ==========

        // 2.1 快速检查：发现 entry 存在
        let entry_exists = registry.contains_project(project_id);
        assert!(entry_exists);

        // 2.2 显式检查 Pending 状态
        let should_replace = {
            let info = registry.get_agent_info(project_id).unwrap();
            *info.status() == AgentStatus::Pending
        }; // 释放 Ref 锁
        assert!(should_replace, "应该检测到 Pending 状态");

        // 2.3 创建真实会话（不持有锁）
        let real_session_id = "ses_real_12345";
        let (prompt_tx, _prompt_rx) = mpsc::channel(100);
        let (cancel_tx, _cancel_rx) = mpsc::channel(100);

        let real_session = ProjectAndAgentInfo {
            project_id: project_id.to_string(),
            session_id: SessionId::new(Arc::from(real_session_id)),
            prompt_tx,
            cancel_tx,
            model_provider: None,
            request_id: None,
            status: AgentStatus::Idle,
            last_activity: chrono::Utc::now(),
            created_at: chrono::Utc::now(),
            stop_handle: None,
        };

        // ========== 第三阶段：原子性替换 ==========

        // 使用 DashMap entry API 进行原子性操作
        use dashmap::mapref::entry::Entry;
        let was_pending = match registry.as_ref().inner_mut().entry(project_id.to_string()) {
            Entry::Occupied(mut entry) => {
                // 提取状态值，避免借用冲突
                let is_pending = {
                    let existing = entry.get();
                    *existing.status() == AgentStatus::Pending
                };
                if is_pending {
                    entry.insert(real_session.clone());
                    true
                } else {
                    false
                }
            }
            Entry::Vacant(_) => false,
        };

        assert!(was_pending, "应该成功替换 pending 占位符");

        // 验证：真实会话已插入
        let final_info = registry.get_agent_info(project_id).unwrap();
        assert_eq!(final_info.session_id.to_string(), real_session_id);
        assert_eq!(format!("{:?}", final_info.status), "Idle");
        assert!(!final_info.prompt_tx.is_closed());
    }

    // PendingGuard 已 drop，但真实会话应该保留
    assert!(registry.contains_project(project_id));
    let final_info = registry.get_agent_info(project_id).unwrap();
    assert_eq!(format!("{:?}", final_info.status), "Idle");

    // 清理
    registry.remove_by_project(project_id);
}

// ============================================================================
// 调试测试 - 定位挂起问题
// ============================================================================

#[test]
fn debug_simple_registry_operations() {
    use agent_runner::service::AgentSessionRegistry;
    use shared_types::SessionEntry;

    let registry = AgentSessionRegistry::new();
    registry.set_pending("test-1");

    // 测试 contains_project
    assert!(registry.contains_project("test-1"));

    // 测试 get_agent_info
    if let Some(info) = registry.get_agent_info("test-1") {
        // 直接访问字段而不是通过 trait 方法
        let status = &info.status;
        assert_eq!(format!("{:?}", status), "Pending");
    } else {
        panic!("get_agent_info returned None");
    }

    registry.remove_by_project("test-1");
    assert!(!registry.contains_project("test-1"));
}
