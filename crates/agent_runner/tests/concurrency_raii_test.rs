//! PendingGuard RAII 设计测试
//!
//! 验证以下功能：
//! 1. PendingGuard RAII 自动清理

use agent_runner::service::AgentSessionRegistry;
use agent_runner::service::PendingGuard;

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
    }
    // guard 已 drop，Pending 状态应该被清理
    assert!(!registry.contains_project("test-project"));
}

#[test]
fn test_pending_guard_early_return_cleanup() {
    let registry = AgentSessionRegistry::new();

    // 模拟早期返回场景
    let early_return = || {
        let _guard = PendingGuard::new(&registry, "test-project");
        // 早期返回（模拟错误场景）
        false
    };

    // 调用后，guard 已经被 drop，应该被清理
    early_return();

    assert!(!registry.contains_project("test-project"));
}

// ============================================================================
// 2. RAII 快速销毁测试
// ============================================================================

#[test]
fn test_raii_fast_destruction() {
    let registry = AgentSessionRegistry::new();
    let num_guards = 100;

    let start = std::time::Instant::now();

    // 创建大量 guard
    for i in 0..num_guards {
        let project_id = format!("project-{}", i);
        let _guard = PendingGuard::new(&registry, &project_id);
        // guard 立即被 drop
    }

    let elapsed = start.elapsed();

    // 验证: 销毁应该很快（< 50ms）
    assert!(
        elapsed.as_millis() < 50,
        "RAII 销毁应该快速完成，实际耗时: {:?}",
        elapsed
    );

    // 验证: 所有项目都被清理
    assert_eq!(registry.stats().agent_count, 0);
}

// ============================================================================
// 3. PendingGuard 基本工作流测试
// ============================================================================

#[test]
fn test_pending_guard_basic_workflow() {
    let registry = AgentSessionRegistry::new();
    let project_id = "test-pending-workflow";

    // 创建 PendingGuard
    let _guard = PendingGuard::new(&registry, project_id);

    // 验证 Pending 状态
    assert!(registry.contains_project(project_id));

    // guard drop 后清理
    drop(_guard);
    assert!(!registry.contains_project(project_id));
}

// ============================================================================
// 4. 调试测试
// ============================================================================

#[test]
fn debug_simple_registry_operations() {
    let registry = AgentSessionRegistry::new();
    registry.set_pending("test-1");

    // 测试 contains_project
    assert!(registry.contains_project("test-1"));

    registry.remove_by_project("test-1");
    assert!(!registry.contains_project("test-1"));
}
