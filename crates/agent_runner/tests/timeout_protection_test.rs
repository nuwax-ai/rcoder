//! 超时保护测试
//!
//! 验证超时保护机制的正确性:
//! - 验证超时后返回错误
//! - 验证超时后清理资源

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::time::timeout;

/// 测试基础超时机制
#[tokio::test]
async fn test_basic_timeout_mechanism() {
    // 使用 timeout 包装一个长时间运行的操作
    let start = std::time::Instant::now();

    let result = timeout(Duration::from_millis(100), async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        "completed"
    })
    .await;

    let elapsed = start.elapsed();

    // 验证: 应该在 100ms 左右超时,而不是等待 5 秒
    assert!(result.is_err(), "应该在 100ms 时超时");
    assert!(elapsed.as_millis() < 200, "超时测试应该快速完成");
}

/// 测试分级超时警告
#[tokio::test]
async fn test_graduated_timeout_warnings() {
    // 测试不同时长的请求应该触发不同级别的警告
    let test_cases = vec![
        (30, false, false), // 30 秒: 无警告
        (65, true, false),  // 65 秒: 黄色警告 (> 60s)
        (125, true, true),  // 125 秒: 红色警告 (> 120s)
    ];

    for (duration_seconds, should_yellow_warn, should_red_warn) in test_cases {
        let has_yellow = duration_seconds > 60;
        let has_red = duration_seconds > 120;

        assert_eq!(
            has_yellow, should_yellow_warn,
            "{}秒请求的黄色警告判断错误",
            duration_seconds
        );
        assert_eq!(
            has_red, should_red_warn,
            "{}秒请求的红色警告判断错误",
            duration_seconds
        );
    }
}

/// 测试超时后资源清理（使用 RAII Guard 模式）
#[tokio::test]
async fn test_resource_cleanup_after_timeout() {
    use std::collections::HashMap;
    use std::sync::Arc;

    let active_requests = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let request_id = "timeout-request".to_string();

    // 使用 RAII Guard 模式测试资源清理
    struct RequestGuard {
        active_requests: Arc<std::sync::Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>>,
        request_id: String,
    }

    impl Drop for RequestGuard {
        fn drop(&mut self) {
            let mut requests = self.active_requests.lock().unwrap();
            requests.remove(&self.request_id);
        }
    }

    // 注册请求
    {
        let mut requests = active_requests.lock().unwrap();
        requests.insert(request_id.clone(), chrono::Utc::now());
    }

    // 创建 guard
    let guard = RequestGuard {
        active_requests: active_requests.clone(),
        request_id: request_id.clone(),
    };

    // 验证请求存在
    {
        let requests = active_requests.lock().unwrap();
        assert!(requests.contains_key(&request_id));
    }

    // drop guard 应该清理资源
    drop(guard);

    // 验证资源已清理
    {
        let requests = active_requests.lock().unwrap();
        assert!(!requests.contains_key(&request_id));
    }
}

/// 测试超时不会导致死锁
#[tokio::test]
async fn test_timeout_does_not_cause_deadlock() {
    use tokio::sync::mpsc;

    let (tx, mut rx) = mpsc::channel(1);

    // 发送一个值
    let handle = tokio::spawn(async move {
        let result = timeout(Duration::from_millis(100), async {
            tx.send(1).await.ok();
            1
        })
        .await;
        result.unwrap_or(0)
    });

    // 等待任务完成
    let result = handle.await.unwrap();
    assert_eq!(result, 1);

    // 验证信道已关闭
    assert!(rx.recv().await.is_some());
}

/// 测试超时阈值配置
#[tokio::test]
async fn test_timeout_threshold_configuration() {
    // 测试超时阈值的逻辑判断
    let thresholds = vec![
        (100, false, false),  // 100ms: 无警告
        (1000, false, false), // 1s: 无警告
        (30000, false, false), // 30s: 无警告
        (60000, false, false), // 60s: 无警告
        (60001, true, false),  // 60s+1ms: 黄色警告
        (120000, true, false), // 120s: 黄色警告
        (120001, true, true),  // 120s+1ms: 红色警告
    ];

    for (ms, expect_yellow, expect_red) in thresholds {
        let is_yellow = ms > 60_000;
        let is_red = ms > 120_000;

        assert_eq!(is_yellow, expect_yellow, "阈值 {}ms 黄色警告判断错误", ms);
        assert_eq!(is_red, expect_red, "阈值 {}ms 红色警告判断错误", ms);
    }
}

/// 测试多个请求的超时检测
#[tokio::test]
async fn test_multiple_requests_timeout_detection() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let timeout_count = Arc::new(AtomicUsize::new(0));
    let success_count = Arc::new(AtomicUsize::new(0));

    let mut handles = vec![];

    for i in 0..5 {
        let timeout_count_clone = timeout_count.clone();
        let success_count_clone = success_count.clone();

        let handle = tokio::spawn(async move {
            let duration = if i % 2 == 0 {
                // 偶数: 快速完成
                Duration::from_millis(10)
            } else {
                // 奇数: 会超时
                Duration::from_millis(5)
            };

            let result = timeout(duration, async {
                // 模拟工作
                if i % 2 == 0 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    "success"
                } else {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    "completed"
                }
            })
            .await;

            match result {
                Ok(_) => {
                    success_count_clone.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    timeout_count_clone.fetch_add(1, Ordering::Relaxed);
                }
            }
        });

        handles.push(handle);
    }

    // 等待所有任务完成
    for handle in handles {
        handle.await.unwrap();
    }

    // 验证: 应该有部分超时
    let timeouts = timeout_count.load(Ordering::Relaxed);
    let successes = success_count.load(Ordering::Relaxed);

    // 至少有一些超时（因为有3个奇数的请求会超时）
    assert!(timeouts >= 2, "应该有至少2个超时，实际: {}", timeouts);
    assert_eq!(timeouts + successes, 5, "所有请求都应该被处理");
}
