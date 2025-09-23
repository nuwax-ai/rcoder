//! Claude Code ACP 连接示例
//!
//! 这个示例展示了如何使用 Claude Code ACP 连接器自动检查安装并连接到 claude-code-acp 服务。

use claude::{
    ClaudeCodeAcpConfig, ClaudeCodeAcpConnectionManager, ClaudeCodeAcpConnector,
};
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 初始化日志
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    println!("🚀 Claude Code ACP 连接示例");

    // 1. 创建连接器
    let config = ClaudeCodeAcpConfig::default();
    let connector = ClaudeCodeAcpConnector::new(config);

    println!("📋 配置信息:");
    println!("  包名: {}", connector.config().package_name);
    println!("  二进制名称: {}", connector.config().binary_name);

    // 2. 创建连接管理器
    let manager = ClaudeCodeAcpConnectionManager::default();

    // 3. 创建连接并启动会话
    println!("\n🔗 正在连接到 Claude Code ACP 服务...");
    match manager.create_connection_with_session(None).await {
        Ok((mut connection, session_response)) => {
            println!("✅ 连接成功!");
            println!("  会话ID: {}", session_response.session_id);

            // 4. 订阅流消息
            let mut subscriber = connection.subscribe();

            // 5. 在后台处理流消息
            tokio::spawn(async move {
                while let Ok(message) = subscriber.recv().await {
                    println!("📨 收到流消息: {:?}", message.direction);
                    // 这里可以处理不同类型的消息
                }
            });

            // 6. 发送测试提示
            println!("\n💬 发送测试提示...");
            match connection
                .prompt(
                    session_response.session_id.clone(),
                    vec!["Hello, Claude! 请介绍一下你自己。".to_string()],
                )
                .await
            {
                Ok(response) => {
                    println!("✅ 提示发送成功!");
                    println!("  停止原因: {:?}", response.stop_reason);
                }
                Err(e) => {
                    println!("❌ 提示发送失败: {}", e);
                }
            }

            // 7. 等待一段时间以接收响应
            println!("\n⏳ 等待响应...");
            sleep(Duration::from_secs(5)).await;

            // 8. 关闭连接
            println!("\n🔚 关闭连接...");
            connection.close().await?;
            println!("✅ 连接已关闭");
        }
        Err(e) => {
            println!("❌ 连接失败: {}", e);
            println!("💡 这可能是因为:");
            println!("   1. Node.js 未安装");
            println!("   2. 网络连接问题");
            println!("   3. claude-code-acp 包无法安装");
        }
    }

    Ok(())
}