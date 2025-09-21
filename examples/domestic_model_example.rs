use acp_adapter::{AcpAdapter, AcpConfig};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    println!("🚀 国内大模型 ACP 集成示例");
    println!("================================");

    // 示例1: 直接配置国内大模型
    let config1 = AcpConfig::domestic_model(
        "claude".to_string(),
        "python".to_string(),
        "your_domestic_api_key".to_string(),
        "https://api.domestic-provider.com/v1".to_string(),
    );

    println!("📋 直接配置示例:");
    println!("  代理类型: {}", config1.agent_type);
    println!("  命令: {}", config1.process.command);
    match &config1.authentication {
        acp_adapter::config::AuthenticationMethod::ApiKey { key, base_url, .. } => {
            println!("  API Key: {}...", &key[..8.min(key.len())]);
            println!("  Base URL: {:?}", base_url);
        }
        _ => {}
    }

    // 示例2: 从环境变量配置
    println!("\n📋 环境变量配置示例:");
    println!("  设置环境变量:");
    println!("    export API_KEY=your_domestic_api_key");
    println!("    export BASE_URL=https://api.domestic-provider.com/v1");
    
    match AcpConfig::domestic_model_from_env("claude".to_string(), "python".to_string()) {
        Ok(config2) => {
            println!("  ✅ 从环境变量成功创建配置");
            println!("  代理类型: {}", config2.agent_type);
            println!("  命令: {}", config2.process.command);
            match &config2.authentication {
                acp_adapter::config::AuthenticationMethod::ApiKey { key, base_url, .. } => {
                    println!("  API Key: {}...", &key[..8.min(key.len())]);
                    println!("  Base URL: {:?}", base_url);
                }
                _ => {}
            }
        }
        Err(e) => {
            println!("  ⚠️  从环境变量创建配置失败: {}", e);
            println!("  💡 请设置 API_KEY 和 BASE_URL 环境变量");
        }
    }

    // 示例3: 自定义配置
    println!("\n📋 自定义配置示例:");
    let config3 = AcpConfig::new("custom_model".to_string(), "custom_command".to_string())
        .with_custom_api_key_auth(
            "custom_key".to_string(),
            Some("X-API-Key".to_string()),  // 自定义 header
            Some("https://custom.api.com".to_string()),
        )
        .with_working_dir(PathBuf::from("."))
        .with_timeout(120);

    println!("  代理类型: {}", config3.agent_type);
    println!("  命令: {}", config3.process.command);
    println!("  超时: {:?} 秒", config3.process.timeout_seconds);
    match &config3.authentication {
        acp_adapter::config::AuthenticationMethod::ApiKey { key, header_name, base_url, .. } => {
            println!("  API Key: {}...", &key[..8.min(key.len())]);
            println!("  Header: {:?}", header_name);
            println!("  Base URL: {:?}", base_url);
        }
        _ => {}
    }

    println!("\n🎯 使用建议:");
    println!("  1. 对于国内大模型，使用 AcpConfig::domestic_model()");
    println!("  2. 设置环境变量 API_KEY 和 BASE_URL");
    println!("  3. 使用 with_custom_api_key_auth() 进行完全自定义");
    println!("  4. 确保 base_url 指向正确的国内 API 端点");

    Ok(())
}
