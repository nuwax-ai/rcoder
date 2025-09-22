//! 真正的 ACP 协议集成示例
//!
//! 展示如何使用真正的 ACP 协议与 codex agent 通信
//! 基于对 agent-client-protocol 源码的深入分析

use anyhow::Result;
use std::path::PathBuf;
use tokio::time::{sleep, Duration};
use tracing::{info, warn, error};
use tracing_subscriber;

use rcoder::acp_client::{
    AcpClientConfig, AcpClient, AcpConnectionManager,
    create_codex_acp_connection, send_prompt_to_codex,
};
use rcoder::codex_acp_mpmc::{GlobalCodexManager, send_prompt_global};

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    println!("🚀 真正的 ACP 协议集成示例");
    println!("================================");

    // 检查环境变量
    let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_else(|_| {
        warn!("⚠️ ANTRHOPIC_API_KEY 环境变量未设置");
        "dummy_key".to_string()
    });

    if api_key == "dummy_key" {
        println!("⚠️ 警告: 使用虚拟 API 密钥，真实的 ACP 通信将失败");
        println!("💡 请设置 ANTHROPIC_API_KEY 环境变量以进行真实测试");
    }

    // 示例 1: 直接使用 ACP 客户端
    println!("\n📋 示例 1: 直接使用 ACP 客户端");
    println!("================================");
    if let Err(e) = demonstrate_direct_acp_client(&api_key).await {
        error!("❌ 直接 ACP 客户端示例失败: {}", e);
    }

    // 示例 2: 使用 ACP 连接管理器
    println!("\n📋 示例 2: 使用 ACP 连接管理器");
    println!("================================");
    if let Err(e) = demonstrate_acp_connection_manager(&api_key).await {
        error!("❌ ACP 连接管理器示例失败: {}", e);
    }

    // 示例 3: 使用全局 MPMC 管理器
    println!("\n📋 示例 3: 使用全局 MPMC 管理器");
    println!("================================");
    if let Err(e) = demonstrate_global_mpmc_manager(&api_key).await {
        error!("❌ 全局 MPMC 管理器示例失败: {}", e);
    }

    println!("\n🎉 所有示例执行完成！");
    println!("💡 要运行真实的 ACP 通信，请确保:");
    println!("   1. 已安装 claude-code 命令行工具");
    println!("   2. 设置了有效的 ANTHROPIC_API_KEY");
    println!("   3. 网络连接正常");

    Ok(())
}

/// 示例 1: 直接使用 ACP 客户端
async fn demonstrate_direct_acp_client(api_key: &str) -> Result<()> {
    println!("🔧 创建 ACP 客户端配置...");

    let config = AcpClientConfig::for_codex()
        .with_api_key(api_key.to_string())
        .with_working_dir(PathBuf::from("."));

    println!("📋 配置信息:");
    println!("   代理命令: {}", config.codex_command);
    println!("   代理参数: {:?}", config.codex_args);
    println!("   工作目录: {:?}", config.working_dir);
    println!("   文件读取: {}", config.client_capabilities.fs.read_text_file);
    println!("   文件写入: {}", config.client_capabilities.fs.write_text_file);

    let mut client = AcpClient::new(config);

    println!("\n🔌 尝试初始化 ACP 连接...");
    match client.initialize().await {
        Ok(_) => {
            println!("✅ ACP 连接初始化成功");

            println!("\n📝 尝试创建会话...");
            match client.create_session().await {
                Ok(session) => {
                    println!("✅ 会话创建成功，ID: {}", session.session_id);
                    println!("   工作目录: {:?}", session.working_dir);
                    println!("   活跃状态: {}", session.is_active);

                    println!("\n💬 发送测试提示...");
                    let test_prompt = "Hello, this is a test message from ACP client!";
                    match client.send_prompt(test_prompt).await {
                        Ok(response) => {
                            println!("✅ 提示发送成功");
                            println!("📝 响应:\n{}", response);
                        }
                        Err(e) => {
                            println!("⚠️ 提示发送失败: {}", e);
                            println!("💡 这可能是因为 claude-code 未安装或 API 密钥无效");
                        }
                    }
                }
                Err(e) => {
                    println!("⚠️ 会话创建失败: {}", e);
                }
            }
        }
        Err(e) => {
            println!("⚠️ ACP 连接初始化失败: {}", e);
            println!("💡 请确保 claude-code 已安装并且可访问");
        }
    }

    // 清理
    client.close().await?;
    Ok(())
}

/// 示例 2: 使用 ACP 连接管理器
async fn demonstrate_acp_connection_manager(api_key: &str) -> Result<()> {
    println!("🔧 创建 ACP 连接管理器...");

    let config = AcpClientConfig::for_codex()
        .with_api_key(api_key.to_string())
        .with_working_dir(PathBuf::from("."));

    let mut manager = AcpConnectionManager::new(config);

    println!("\n💬 发送测试提示到代理...");
    let test_prompt = "Hello from ACP connection manager!";
    match manager.send_prompt(test_prompt).await {
        Ok(response) => {
            println!("✅ 提示发送成功");
            println!("📝 响应:\n{}", response);
        }
        Err(e) => {
            println!("⚠️ 提示发送失败: {}", e);
            println!("💡 这可能是因为 claude-code 未安装或 API 密钥无效");
        }
    }

    // 清理
    manager.close().await?;
    Ok(())
}

/// 示例 3: 使用全局 MPMC 管理器
async fn demonstrate_global_mpmc_manager(api_key: &str) -> Result<()> {
    println!("🔧 使用全局 MPMC 管理器...");

    let project_id = "acp_test_project";
    let test_prompt = "Hello from global MPMC manager!";

    println!("\n💬 发送测试提示到项目 '{}'...", project_id);
    match send_prompt_global(project_id, test_prompt).await {
        Ok(response) => {
            println!("✅ 提示发送成功");
            println!("📝 响应:\n{}", response);
        }
        Err(e) => {
            println!("⚠️ 提示发送失败: {}", e);
            println!("💡 这可能是因为 claude-code 未安装或 API 密钥无效");
        }
    }

    // 检查服务状态
    if let Some(status) = rcoder::codex_acp_mpmc::get_service_status(project_id).await {
        println!("📊 服务状态: {}", status);
    } else {
        println!("📊 服务不存在");
    }

    Ok(())
}

/// 额外的示例: 展示错误处理和重试逻辑
async fn demonstrate_error_handling(api_key: &str) -> Result<()> {
    println!("🔧 错误处理示例...");

    let config = AcpClientConfig::for_codex()
        .with_api_key("invalid_key".to_string())  // 故意使用无效密钥
        .with_working_dir(PathBuf::from("/nonexistent/path"));

    let mut client = AcpClient::new(config);

    println!("📝 尝试使用无效配置连接...");
    match client.initialize().await {
        Ok(_) => {
            println!("✅ 意外成功");
        }
        Err(e) => {
            println!("⚠️ 预期的失败: {}", e);

            // 重试逻辑示例
            println!("🔄 尝试重试...");
            sleep(Duration::from_secs(1)).await;

            let retry_config = AcpClientConfig::for_codex()
                .with_api_key(api_key.to_string())
                .with_working_dir(PathBuf::from("."));

            let mut retry_client = AcpClient::new(retry_config);

            if let Ok(_) = retry_client.initialize().await {
                println!("✅ 重试成功");
            } else {
                println!("⚠️ 重试仍然失败");
            }
        }
    }

    Ok(())
}

/// 性能测试示例
async fn performance_test(api_key: &str) -> Result<()> {
    println!("🔧 性能测试示例...");

    let config = AcpClientConfig::for_codex()
        .with_api_key(api_key.to_string())
        .with_working_dir(PathBuf::from("."));

    let mut manager = AcpConnectionManager::new(config);

    let prompts = vec![
        "What is 2+2?",
        "Explain Rust's ownership system briefly.",
        "What's the weather like?",
        "Count to 10.",
    ];

    println!("📊 发送 {} 个提示...", prompts.len());

    let start_time = std::time::Instant::now();

    for (i, prompt) in prompts.iter().enumerate() {
        println!("📝 提示 {}: {}", i + 1, prompt);

        match manager.send_prompt(prompt).await {
            Ok(response) => {
                println!("✅ 提示 {} 完成", i + 1);
                // 只显示响应的前 100 个字符
                let preview = response.chars().take(100).collect::<String>();
                println!("📄 预览: {}...", preview);
            }
            Err(e) => {
                println!("⚠️ 提示 {} 失败: {}", i + 1, e);
            }
        }
    }

    let duration = start_time.elapsed();
    println!("📊 性能测试完成，耗时: {:?}", duration);

    manager.close().await?;
    Ok(())
}