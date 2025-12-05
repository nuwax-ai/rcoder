//! ACP 连接配置示例 - 包含最小保护时间
//!
//! 本示例展示如何使用 AcpConnectionConfig 配置最小保护时间，
//! 以防止新创建的容器在并发场景下被过早清理。

use agent_abstraction::acp::AcpConnectionConfig;
use std::time::Duration;

fn main() {
    println!("=== ACP 连接配置示例 ===\n");

    // 示例 1: 使用默认配置（包含 5 分钟最小保护时间）
    let default_config = AcpConnectionConfig::default();
    println!("默认配置:");
    println!(
        "  - 最大空闲时间: {:?} (5分钟)",
        default_config.max_idle_time
    );
    println!(
        "  - 清理间隔: {:?} (1分钟)",
        default_config.cleanup_interval
    );
    println!(
        "  - 最小保护时间: {:?} (5分钟) ⭐",
        default_config.min_protection_duration
    );
    println!(
        "  - 连接超时: {:?} (30秒)",
        default_config.connection_timeout
    );
    println!("  - 最大连接数: {}\n", default_config.max_connections);

    // 示例 2: 自定义保护时间（10分钟）
    let custom_config = AcpConnectionConfig::default()
        .with_max_idle_time(Duration::from_secs(600)) // 10分钟空闲超时
        .with_cleanup_interval(Duration::from_secs(120)) // 2分钟清理间隔
        .with_min_protection_duration(Duration::from_secs(600)) // 10分钟最小保护
        .with_connection_timeout(Duration::from_secs(60)) // 1分钟连接超时
        .with_max_connections(50);

    println!("自定义配置:");
    println!(
        "  - 最大空闲时间: {:?} (10分钟)",
        custom_config.max_idle_time
    );
    println!("  - 清理间隔: {:?} (2分钟)", custom_config.cleanup_interval);
    println!(
        "  - 最小保护时间: {:?} (10分钟) ⭐",
        custom_config.min_protection_duration
    );
    println!(
        "  - 连接超时: {:?} (1分钟)",
        custom_config.connection_timeout
    );
    println!("  - 最大连接数: {}\n", custom_config.max_connections);

    // 示例 3: 从环境变量加载配置
    println!("从环境变量加载配置示例:");
    println!("  设置以下环境变量:");
    println!("    ACP_MAX_IDLE_TIME=900        # 15分钟空闲超时");
    println!("    ACP_CLEANUP_INTERVAL=180     # 3分钟清理间隔");
    println!("    ACP_MIN_PROTECTION_DURATION=600  # 10分钟最小保护 ⭐");
    println!("    ACP_CONNECTION_TIMEOUT=60    # 1分钟连接超时");
    println!("    ACP_MAX_CONNECTIONS=100      # 最大100个连接\n");

    println!("实际使用:");
    println!("  let config = AcpConnectionConfig::from_env();");
    println!("  // 将根据环境变量创建配置\n");

    // 示例 4: 说明保护时间的重要性
    println!("=== 为什么需要最小保护时间？ ===\n");
    println!("在并发场景下，可能出现以下问题:");
    println!("  1. 容器刚创建完成");
    println!("  2. 立即被清理扫描任务检测到");
    println!("  3. 由于某种原因被认为是空闲的");
    println!("  4. 被提前销毁\n");

    println!("解决方案:");
    println!("  - 设置 min_protection_duration = 5分钟");
    println!("  - 新创建的容器在5分钟内不会被清理");
    println!("  - 即使出现并发问题，容器也会受到保护\n");

    println!("参考实现 (来自 cleanup_task.rs):");
    println!("  🛡️ 最小保护时间：容器创建后5分钟内不会被清理");
    println!("  let min_protection_duration = Duration::from_secs(5 * 60);");
    println!("  if current_time.signed_duration_since(created_at) < min_protection_duration {{");
    println!("      continue; // 跳过保护期内的容器");
    println!("  }}\n");

    println!("配置完成！ ✅");
}
