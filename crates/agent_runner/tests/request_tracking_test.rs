//! 请求追踪集成测试
//!
//! 测试 AgentWorkerManager 的请求追踪功能:
//! - 请求开始时插入追踪记录
//! - 请求完成时移除追踪记录
//! - DoS 防护（超过上限拒绝请求）
//! - 活跃请求状态查询

use agent_runner::agent_worker_manager::{AgentWorkerManager, MAX_ACTIVE_REQUESTS};

/// 测试请求追踪的基本流程
///
/// 验证:
/// 1. 请求开始时, track_request_start 成功
/// 2. get_active_requests_summary 返回正确的计数
/// 3. 请求完成时, track_request_complete 正确清理
#[tokio::test]
async fn test_request_tracking_basic_flow() {
    let (manager, _heartbeat_rx, _ready_rx, _heartbeat_tx, _ready_tx) = AgentWorkerManager::new();

    let request_id = "test-request-123".to_string();

    // 1. 请求开始: 插入追踪记录
    let result = manager.track_request_start(request_id.clone());
    assert!(result.is_ok(), "track_request_start 应该成功");

    // 2. 验证追踪状态
    let summary = manager.get_active_requests_summary();
    assert_eq!(summary.count, 1, "应该有 1 个活跃请求");

    // 3. 请求完成: 移除追踪记录
    manager.track_request_complete(&request_id);

    let summary = manager.get_active_requests_summary();
    assert_eq!(summary.count, 0, "活跃请求应该被清空");
}

/// 测试多个并发请求的追踪
#[tokio::test]
async fn test_multiple_concurrent_requests_tracking() {
    let (manager, _heartbeat_rx, _ready_rx, _heartbeat_tx, _ready_tx) = AgentWorkerManager::new();

    let request_ids = vec![
        "req-1".to_string(),
        "req-2".to_string(),
        "req-3".to_string(),
    ];

    // 插入多个请求
    for req_id in &request_ids {
        let result = manager.track_request_start(req_id.clone());
        assert!(result.is_ok());
    }

    let summary = manager.get_active_requests_summary();
    assert_eq!(summary.count, 3, "应该有 3 个活跃请求");

    // 逐个移除
    for (i, req_id) in request_ids.iter().enumerate() {
        manager.track_request_complete(req_id);
        let summary = manager.get_active_requests_summary();
        assert_eq!(summary.count, 3 - i - 1, "活跃请求数应该减少");
    }

    let summary = manager.get_active_requests_summary();
    assert_eq!(summary.count, 0, "所有请求应该被清空");
}

/// 测试 DoS 防护 - 超过上限时拒绝请求
#[tokio::test]
async fn test_dos_protection_limit() {
    let (manager, _heartbeat_rx, _ready_rx, _heartbeat_tx, _ready_tx) = AgentWorkerManager::new();

    // 插入 MAX_ACTIVE_REQUESTS 个请求
    for i in 0..MAX_ACTIVE_REQUESTS {
        let result = manager.track_request_start(format!("req-{}", i));
        assert!(result.is_ok(), "第 {} 个请求应该成功", i);
    }

    let summary = manager.get_active_requests_summary();
    assert_eq!(summary.count, MAX_ACTIVE_REQUESTS);

    // 超过限制应该被拒绝
    let result = manager.track_request_start("overflow-request".to_string());
    assert!(result.is_err(), "超过限制的请求应该被拒绝");

    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("已达上限"),
        "错误消息应该包含 '已达上限': {}",
        err_msg
    );

    // 验证数量未增加
    let summary = manager.get_active_requests_summary();
    assert_eq!(summary.count, MAX_ACTIVE_REQUESTS);
}

/// 测试请求完成后可以添加新请求
#[tokio::test]
async fn test_request_slot_reuse_after_completion() {
    let (manager, _heartbeat_rx, _ready_rx, _heartbeat_tx, _ready_tx) = AgentWorkerManager::new();

    // 插入到上限
    for i in 0..MAX_ACTIVE_REQUESTS {
        manager.track_request_start(format!("req-{}", i)).unwrap();
    }

    // 完成一个请求
    manager.track_request_complete("req-0");

    // 现在应该可以添加新请求
    let result = manager.track_request_start("new-request".to_string());
    assert!(result.is_ok(), "完成请求后应该可以添加新请求");

    let summary = manager.get_active_requests_summary();
    assert_eq!(summary.count, MAX_ACTIVE_REQUESTS);
}

/// 测试活跃请求状态查询
#[tokio::test]
async fn test_active_requests_query() {
    let (manager, _heartbeat_rx, _ready_rx, _heartbeat_tx, _ready_tx) = AgentWorkerManager::new();

    // 空状态
    let summary = manager.get_active_requests_summary();
    assert_eq!(summary.count, 0);
    assert_eq!(summary.max_duration_secs, 0);
    assert_eq!(summary.timeout_count_60, 0);
    assert_eq!(summary.timeout_count_120, 0);

    // 插入请求
    manager.track_request_start("req-1".to_string()).unwrap();

    let summary = manager.get_active_requests_summary();
    assert_eq!(summary.count, 1);

    // 查询所有请求（调试用）
    let active = manager.get_active_requests();
    assert!(active.contains_key("req-1"));
    assert!(!active.contains_key("req-2"));
}

/// 测试清除所有活跃请求
#[tokio::test]
async fn test_clear_active_requests() {
    let (manager, _heartbeat_rx, _ready_rx, _heartbeat_tx, _ready_tx) = AgentWorkerManager::new();

    // 插入多个请求
    for i in 0..5 {
        manager.track_request_start(format!("req-{}", i)).unwrap();
    }

    let summary = manager.get_active_requests_summary();
    assert_eq!(summary.count, 5);

    // 清除所有
    manager.clear_active_requests();

    let summary = manager.get_active_requests_summary();
    assert_eq!(summary.count, 0);
}

/// 测试重复追踪同一请求 ID
#[tokio::test]
async fn test_duplicate_request_id_tracking() {
    let (manager, _heartbeat_rx, _ready_rx, _heartbeat_tx, _ready_tx) = AgentWorkerManager::new();

    let request_id = "duplicate-req".to_string();

    // 第一次追踪
    manager.track_request_start(request_id.clone()).unwrap();
    let summary = manager.get_active_requests_summary();
    assert_eq!(summary.count, 1);

    // 重复追踪同一 ID（应该覆盖，数量不变）
    manager.track_request_start(request_id.clone()).unwrap();
    let summary = manager.get_active_requests_summary();
    assert_eq!(summary.count, 1, "重复 ID 应该覆盖而非增加");

    // 完成一次就清除
    manager.track_request_complete(&request_id);
    let summary = manager.get_active_requests_summary();
    assert_eq!(summary.count, 0);
}

/// 测试完成不存在的请求 ID
#[tokio::test]
async fn test_complete_nonexistent_request() {
    let (manager, _heartbeat_rx, _ready_rx, _heartbeat_tx, _ready_tx) = AgentWorkerManager::new();

    // 完成一个不存在的请求（应该不报错）
    manager.track_request_complete("nonexistent-req");

    let summary = manager.get_active_requests_summary();
    assert_eq!(summary.count, 0);
}
