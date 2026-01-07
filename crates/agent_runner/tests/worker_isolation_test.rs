//! Worker 隔离测试
//!
//! 验证: 一个 Worker 阻塞不应影响其他 Agent
//!
//! 测试场景:
//! - Worker 1 阻塞时,Worker 2 可以正常处理请求
//! - 每个 Worker 有独立的请求追踪状态
//! - Worker 重启不影响其他 Worker

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::timeout;

/// 测试 Worker 隔离性 - 基础测试
///
/// 验证两个独立的 Worker 管理器不会互相干扰
#[tokio::test]
async fn test_worker_isolation_basic() {
    // 模拟两个 Worker 的独立追踪状态
    let worker1_requests: Arc<Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let worker2_requests: Arc<Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Worker 1 添加请求
    {
        let mut reqs = worker1_requests.lock().unwrap();
        reqs.insert("worker1-req-1".to_string(), chrono::Utc::now());
    }

    // Worker 2 添加请求
    {
        let mut reqs = worker2_requests.lock().unwrap();
        reqs.insert("worker2-req-1".to_string(), chrono::Utc::now());
    }

    // 验证独立性
    {
        let reqs1 = worker1_requests.lock().unwrap();
        let reqs2 = worker2_requests.lock().unwrap();

        assert_eq!(reqs1.len(), 1);
        assert_eq!(reqs2.len(), 1);
        assert!(reqs1.contains_key("worker1-req-1"));
        assert!(reqs2.contains_key("worker2-req-1"));
        assert!(!reqs1.contains_key("worker2-req-1"));
        assert!(!reqs2.contains_key("worker1-req-1"));
    }
}

/// 测试 Worker 阻塞时不影响其他 Worker (使用 tokio::spawn 模拟)
///
/// 这个测试验证两个独立的 tokio task 不会互相阻塞
#[tokio::test]
async fn test_blocked_worker_does_not_block_others() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let worker1_completed = Arc::new(AtomicBool::new(false));
    let worker2_completed = Arc::new(AtomicBool::new(false));

    let worker1_completed_clone = worker1_completed.clone();
    let worker2_completed_clone = worker2_completed.clone();

    // Worker 1: 模拟长时间阻塞
    let handle1 = tokio::spawn(async move {
        // 模拟 5 秒的长任务
        tokio::time::sleep(Duration::from_secs(5)).await;
        worker1_completed_clone.store(true, Ordering::SeqCst);
    });

    // Worker 2: 快速任务
    let handle2 = tokio::spawn(async move {
        // 快速完成
        tokio::time::sleep(Duration::from_millis(100)).await;
        worker2_completed_clone.store(true, Ordering::SeqCst);
    });

    // 验证: Worker 2 应该在 1 秒内完成 (不被 Worker 1 阻塞)
    let result = timeout(Duration::from_secs(1), handle2).await;
    assert!(result.is_ok(), "Worker 2 应该快速完成，不被 Worker 1 阻塞");

    // 验证: Worker 2 已完成
    assert!(
        worker2_completed.load(Ordering::SeqCst),
        "Worker 2 应该已完成"
    );

    // 验证: Worker 1 仍在运行
    assert!(
        !worker1_completed.load(Ordering::SeqCst),
        "Worker 1 应该仍在运行"
    );

    // 清理：取消 Worker 1
    handle1.abort();
}

/// 测试阻塞注入功能 (需要 testing feature)
///
/// 注意: testing feature 会自动启用 test-blocking feature
#[tokio::test]
#[cfg(feature = "testing")]
async fn test_blocking_injection() {
    use agent_runner::testing::blocking::{inject_blocking, maybe_block, reset_blocking, BlockingConfig};

    // 默认配置不阻塞
    reset_blocking();

    let start = std::time::Instant::now();
    maybe_block("prompt").await;
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 100,
        "默认配置不应该阻塞"
    );

    // 注入短暂阻塞
    inject_blocking(BlockingConfig {
        block_prompt: true,
        block_duration: Duration::from_millis(200),
        ..Default::default()
    });

    let start = std::time::Instant::now();
    maybe_block("prompt").await;
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() >= 200,
        "应该阻塞至少 200ms"
    );

    // 清理
    reset_blocking();
}

/// 测试 Worker 独立的请求追踪状态
#[tokio::test]
async fn test_independent_request_tracking() {
    // 模拟 WorkerManager 的 get_active_requests 方法
    fn get_active_requests(
        storage: &Arc<Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>>,
    ) -> HashMap<String, chrono::DateTime<chrono::Utc>> {
        storage.lock().unwrap().clone()
    }

    let worker1_storage = Arc::new(Mutex::new(HashMap::new()));
    let worker2_storage = Arc::new(Mutex::new(HashMap::new()));

    // Worker 1 添加多个请求
    {
        let mut reqs = worker1_storage.lock().unwrap();
        reqs.insert("worker1-req-1".to_string(), chrono::Utc::now());
        reqs.insert("worker1-req-2".to_string(), chrono::Utc::now());
        reqs.insert("worker1-req-3".to_string(), chrono::Utc::now());
    }

    // Worker 2 添加一个请求
    {
        let mut reqs = worker2_storage.lock().unwrap();
        reqs.insert("worker2-req-1".to_string(), chrono::Utc::now());
    }

    // 验证独立性
    let active1 = get_active_requests(&worker1_storage);
    let active2 = get_active_requests(&worker2_storage);

    assert_eq!(active1.len(), 3);
    assert_eq!(active2.len(), 1);

    // Worker 1 完成一个请求
    {
        let mut reqs = worker1_storage.lock().unwrap();
        reqs.remove("worker1-req-1");
    }

    let active1_after = get_active_requests(&worker1_storage);
    assert_eq!(active1_after.len(), 2);

    // Worker 2 不受影响
    let active2_after = get_active_requests(&worker2_storage);
    assert_eq!(active2_after.len(), 1);
}

/// 测试 Worker 重启不影响其他 Worker
#[tokio::test]
async fn test_worker_restart_isolation() {
    let worker1_storage = Arc::new(Mutex::new(HashMap::new()));
    let worker2_storage = Arc::new(Mutex::new(HashMap::new()));

    // 两个 Worker 都有活跃请求
    {
        let mut reqs1 = worker1_storage.lock().unwrap();
        let mut reqs2 = worker2_storage.lock().unwrap();
        reqs1.insert("req-1".to_string(), chrono::Utc::now());
        reqs2.insert("req-2".to_string(), chrono::Utc::now());
    }

    // 模拟 Worker 1 重启 (清空追踪)
    {
        let mut reqs1 = worker1_storage.lock().unwrap();
        reqs1.clear();
    }

    // 验证: Worker 1 的请求已清空
    let active1 = worker1_storage.lock().unwrap().clone();
    assert_eq!(active1.len(), 0);

    // 验证: Worker 2 不受影响
    let active2 = worker2_storage.lock().unwrap().clone();
    assert_eq!(active2.len(), 1);
    assert!(active2.contains_key("req-2"));
}

/// 测试并发请求处理（使用真实的 tokio 并发）
#[tokio::test]
async fn test_concurrent_request_handling() {
    let storage = Arc::new(Mutex::new(HashMap::new()));
    let mut handles = vec![];

    // 并发插入 10 个请求
    for i in 0..10 {
        let storage_clone = storage.clone();
        let handle = tokio::spawn(async move {
            let mut reqs = storage_clone.lock().unwrap();
            reqs.insert(format!("req-{}", i), chrono::Utc::now());
        });
        handles.push(handle);
    }

    // 等待所有插入完成
    for handle in handles {
        handle.await.unwrap();
    }

    // 验证: 所有请求都被记录
    let reqs = storage.lock().unwrap();
    assert_eq!(reqs.len(), 10);
}

/// 测试请求清理的正确性
#[tokio::test]
async fn test_request_cleanup_correctness() {
    let storage = Arc::new(Mutex::new(HashMap::new()));

    // 添加请求
    {
        let mut reqs = storage.lock().unwrap();
        reqs.insert("req-1".to_string(), chrono::Utc::now());
        reqs.insert("req-2".to_string(), chrono::Utc::now());
        reqs.insert("req-3".to_string(), chrono::Utc::now());
    }

    assert_eq!(storage.lock().unwrap().len(), 3);

    // 完成请求 1
    {
        let mut reqs = storage.lock().unwrap();
        reqs.remove("req-1");
    }
    assert_eq!(storage.lock().unwrap().len(), 2);

    // 完成请求 2
    {
        let mut reqs = storage.lock().unwrap();
        reqs.remove("req-2");
    }
    assert_eq!(storage.lock().unwrap().len(), 1);

    // 完成请求 3
    {
        let mut reqs = storage.lock().unwrap();
        reqs.remove("req-3");
    }
    assert_eq!(storage.lock().unwrap().len(), 0);
}
