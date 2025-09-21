use acp_adapter::{AcpAdapter, AcpConfig};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    println!("🚀 Claude Code ACP 集成示例");
    println!("================================");

    // 创建 Claude Code 配置（现在使用 claude-code-acp）
    let config = AcpConfig::claude_code()
        .with_working_dir(PathBuf::from("."))
        .with_env("CLAUDE_API_KEY".to_string(), 
                 std::env::var("CLAUDE_API_KEY").unwrap_or_default());

    println!("📋 配置信息:");
    println!("  代理类型: {}", config.agent_type);
    println!("  命令: {}", config.process.command);
    println!("  参数: {:?}", config.process.args);
    println!("  MCP 启用: {}", config.mcp_enabled);
    println!("  工作目录: {:?}", config.process.working_dir);

    // 创建 ACP 适配器
    let adapter = AcpAdapter::new(config);

    println!("\n🔧 初始化 ACP 适配器...");
    match adapter.initialize().await {
        Ok(_) => {
            println!("✅ ACP 适配器初始化成功");
            
            // 创建会话
            println!("\n📝 创建新会话...");
            match adapter.create_session().await {
                Ok(session_handle) => {
                    println!("✅ 会话创建成功，ID: {}", session_handle.id());
                    
                    // 这里可以添加更多的会话操作
                    println!("🎉 Claude Code ACP 集成测试完成！");
                }
                Err(e) => {
                    println!("❌ 创建会话失败: {}", e);
                }
            }
        }
        Err(e) => {
            println!("❌ ACP 适配器初始化失败: {}", e);
            println!("💡 请确保:");
            println!("   1. 已安装 Node.js 和 npm");
            println!("   2. 设置了 CLAUDE_API_KEY 环境变量");
            println!("   3. 网络连接正常");
        }
    }

    Ok(())
}
