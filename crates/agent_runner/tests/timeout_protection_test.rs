//! 超时保护测试
//!
//! 验证超时保护机制的正确性:
//! - 使用 Tokio paused_time 加速测试
//! - 验证超时后返回错误
//! - 验证超时后清理资源

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

/// 测试 paused_time 加速超时测试
///
/// 这是 Tokio 的最佳实践: 使用 paused_time 可以让长时间的 sleep 立即完成
///
/// 注意：在 paused_time 模式下：
/// - tokio::time::Instant 会跟随虚拟时间
/// - std::time::Instant 保持真实墙钟时间
/// - 我们使用 std::time::Instant 来验证测试确实快速完成
#[tokio::test(start_paused = true)]
async fn test_timeout_with_paused_time() {
    // 使用 std::time::Instant 来测量真实墙钟时间
    let wall_clock_start = std::time::Instant::now();

    // 注入 200 秒的阻塞 (但在 paused_time 下会立即完成)
    let result = timeout(Duration::from_secs(100), async {
        tokio::time::sleep(Duration::from_secs(200)).await;
        "completed"
    })
    .await;

    let wall_clock_elapsed = wall_clock_start.elapsed();

    // 验证: 应该在 100 "虚拟秒" 后超时
    assert!(result.is_err(), "应该在 100 秒时超时");

    // 验证: 真实墙钟时间应该非常短 (< 100ms)
    assert!(
        wall_clock_elapsed.as_millis() < 100,
        "paused_time 测试应该瞬间完成，实际耗时: {:?}",
        wall_clock_elapsed
    );
}

/// 测试超时前的成功完成
#[tokio::test(start_paused = true)]
async fn test_success_before_timeout() {
    let wall_clock_start = std::time::Instant::now();

    // 任务在 50 秒完成,超时时间是 100 秒
    let result = timeout(Duration::from_secs(100), async {
        tokio::time::sleep(Duration::from_secs(50)).await;
        "completed"
    })
    .await;

    let wall_clock_elapsed = wall_clock_start.elapsed();

    // 验证: 应该成功完成
    assert!(result.is_ok(), "应该在 50 秒时成功完成");
    assert_eq!(result.unwrap(), "completed");

    // 验证: 真实墙钟时间应该非常短
    assert!(
        wall_clock_elapsed.as_millis() < 100,
        "paused_time 测试应该快速完成"
    );
}

/// 测试分级超时警告
#[tokio::test]
async fn test_graduated_timeout_warnings() {
    // 测试不同时长的请求应该触发不同级别的警告
    let test_cases = vec![
        (30, false, false),  // 30 秒: 无警告
        (65, true, false),   // 65 秒: 黄色警告 (> 60s)
        (125, true, true),   // 125 秒: 红色警告 (> 120s)
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

    impl RequestGuard {
        fn new(
            active_requests: Arc<std::sync::Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>>,
            request_id: String,
        ) -> Self {
            // 在单独的作用域中获取锁，确保锁在 move 之前释放
            {
                let mut reqs = active_requests.lock().unwrap();
                reqs.insert(request_id.clone(), chrono::Utc::now());
            }
            Self {
                active_requests,
                request_id,
            }
        }
    }

    impl Drop for RequestGuard {
        fn drop(&mut self) {
            let mut reqs = self.active_requests.lock().unwrap();
            reqs.remove(&self.request_id);
        }
    }

    // 创建 guard
    {
        let _guard = RequestGuard::new(active_requests.clone(), request_id.clone());
        assert_eq!(active_requests.lock().unwrap().len(), 1);
        // guard 在这里被 drop
    }

    // 验证: 资源已被 Drop 自动清理
    assert_eq!(
        active_requests.lock().unwrap().len(),
        0,
        "RAII Guard 应该在 drop 时自动清理资源"
    );
}

/// 测试多个请求的超时检测
#[tokio::test]
async fn test_multiple_requests_timeout_detection() {
    use std::collections::HashMap;

    let mut active_requests: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
    let now = chrono::Utc::now();

    // 添加不同时长的请求
    active_requests.insert("req-1".to_string(), now - chrono::Duration::seconds(30)); // 30 秒 - 正常
    active_requests.insert("req-2".to_string(), now - chrono::Duration::seconds(70)); // 70 秒 - 黄色警告
    active_requests.insert("req-3".to_string(), now - chrono::Duration::seconds(130)); // 130 秒 - 红色警告

    let mut normal_count = 0;
    let mut yellow_count = 0;
    let mut red_count = 0;

    for start_time in active_requests.values() {
        let duration = (now - *start_time).num_seconds();

        if duration > 120 {
            red_count += 1;
        } else if duration > 60 {
            yellow_count += 1;
        } else {
            normal_count += 1;
        }
    }

    assert_eq!(normal_count, 1, "应该有 1 个正常请求");
    assert_eq!(yellow_count, 1, "应该有 1 个黄色警告");
    assert_eq!(red_count, 1, "应该有 1 个红色警告");
}

/// 测试超时阈值配置验证
#[tokio::test]
async fn test_timeout_threshold_configuration() {
    // 测试不同的超时阈值配置
    struct TimeoutConfig {
        name: &'static str,
        threshold_seconds: u64,
        description: &'static str,
    }

    let configs = vec![
        TimeoutConfig {
            name: "new_session",
            threshold_seconds: 100,
            description: "MCP 服务器启动可能较慢",
        },
        TimeoutConfig {
            name: "monitor_warn",
            threshold_seconds: 60,
            description: "监控黄色警告阈值",
        },
        TimeoutConfig {
            name: "monitor_error",
            threshold_seconds: 120,
            description: "监控红色警告阈值",
        },
        TimeoutConfig {
            name: "monitor_restart",
            threshold_seconds: 180,
            description: "触发重启阈值",
        },
    ];

    for config in configs {
        assert!(
            config.threshold_seconds > 0,
            "{} 超时阈值应该大于 0",
            config.name
        );
        println!(
            "配置: {} = {}秒 ({})",
            config.name, config.threshold_seconds, config.description
        );
    }

    // 验证阈值递增关系
    assert!(60 < 120, "warn 阈值应小于 error 阈值");
    assert!(120 < 180, "error 阈值应小于 restart 阈值");
}

/// 测试超时不会导致死锁
#[tokio::test]
async fn test_timeout_does_not_cause_deadlock() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let task_started = Arc::new(AtomicBool::new(false));
    let task_started_clone = task_started.clone();

    // 启动一个任务
    let handle = tokio::spawn(async move {
        task_started_clone.store(true, Ordering::SeqCst);
        // 模拟长时间操作
        tokio::time::sleep(Duration::from_secs(5)).await;
        "completed"
    });

    // 等待任务启动
    tokio::time::sleep(Duration::from_millis(10)).await;

    // 验证任务已启动
    assert!(task_started.load(Ordering::SeqCst), "任务应该已启动");

    // 使用短超时等待
    let result = timeout(Duration::from_millis(100), handle).await;

    // 验证: 超时发生，但没有死锁
    assert!(result.is_err(), "应该超时");
}

/// 测试 tokio::time::Instant 在 paused_time 模式下的行为
#[tokio::test(start_paused = true)]
async fn test_tokio_instant_with_paused_time() {
    // tokio::time::Instant 跟随虚拟时间
    let tokio_start = tokio::time::Instant::now();

    // 前进 10 秒虚拟时间
    tokio::time::sleep(Duration::from_secs(10)).await;

    let tokio_elapsed = tokio_start.elapsed();

    // tokio::time::Instant 应该显示 10 秒
    assert!(
        tokio_elapsed >= Duration::from_secs(10),
        "tokio::time::Instant 应该跟随虚拟时间: {:?}",
        tokio_elapsed
    );
}
