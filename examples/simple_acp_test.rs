//! 简化的 ACP 测试示例
//!
//! 测试基本的 ACP 客户端功能

use anyhow::Result;
use std::path::PathBuf;
use tokio::task::LocalSet;
use tracing_subscriber;

use rcoder::acp_client::{AcpClientConfig, AcpClient};

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    println!("🧪 简化的 ACP 测试示例");
    println!("======================");

    // 使用 LocalSet 处理 spawn_local 问题
    let local_set = LocalSet::new();
    local_set.run_until(async {
        test_acp_client_config().await;
        test_acp_client_creation().await;
    }).await;

    println!("\n✅ 所有测试完成");
    Ok(())
}

async fn test_acp_client_config() {
    println!("\n📋 测试 ACP 客户端配置...");

    let config = AcpClientConfig::for_codex()
        .with_working_dir(PathBuf::from("/tmp"))
        .with_env("TEST_VAR".to_string(), "test_value".to_string());

    println!("✅ 配置创建成功");
    println!("   代理命令: {}", config.codex_command);
    println!("   代理参数: {:?}", config.codex_args);
    println!("   工作目录: {:?}", config.working_dir);
    println!("   环境变量: {:?}", config.env_vars);
    println!("   文件读取能力: {}", config.client_capabilities.fs.read_text_file);
    println!("   文件写入能力: {}", config.client_capabilities.fs.write_text_file);
    println!("   终端能力: {}", config.client_capabilities.terminal);
}

async fn test_acp_client_creation() {
    println!("\n🔧 测试 ACP 客户端创建...");

    let config = AcpClientConfig::for_codex()
        .with_working_dir(PathBuf::from("."));

    let client = AcpClient::new(config);

    println!("✅ 客户端创建成功");
    println!("   连接状态: {}", client.is_connected());
    println!("   当前会话: {:?}", client.current_session());
}