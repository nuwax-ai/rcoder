use acp_adapter::{AcpAdapter, AcpConfig};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    println!("🚀 自定义模型配置示例");
    println!("================================");

    // 示例1: 使用预定义的 GLM 配置
    println!("📋 示例1: GLM 配置");
    let glm_config = AcpConfig::glm("claude".to_string(), "python".to_string())
        .with_working_dir(PathBuf::from("."))
        .with_timeout(120);

    println!("  代理类型: {}", glm_config.agent_type);
    println!("  命令: {}", glm_config.process.command);
    println!("  模型提供商: {:?}", glm_config.model_provider.as_ref().map(|p| &p.name));
    println!("  模型名称: {:?}", glm_config.model_name);
    if let Some(provider) = &glm_config.model_provider {
        println!("  Base URL: {}", provider.base_url);
        println!("  环境变量: {}", provider.env_key);
    }

    // 示例2: 使用通义千问配置
    println!("\n📋 示例2: 通义千问配置");
    let qwen_config = AcpConfig::qwen("claude".to_string(), "python".to_string())
        .with_working_dir(PathBuf::from("."))
        .with_timeout(120);

    println!("  代理类型: {}", qwen_config.agent_type);
    println!("  命令: {}", qwen_config.process.command);
    println!("  模型提供商: {:?}", qwen_config.model_provider.as_ref().map(|p| &p.name));
    println!("  模型名称: {:?}", qwen_config.model_name);
    if let Some(provider) = &qwen_config.model_provider {
        println!("  Base URL: {}", provider.base_url);
        println!("  环境变量: {}", provider.env_key);
    }

    // 示例3: 使用自定义模型配置
    println!("\n📋 示例3: 自定义模型配置");
    let custom_config = AcpConfig::custom_model(
        "claude".to_string(),
        "python".to_string(),
        "my_provider".to_string(),
        "https://api.my-provider.com/v1".to_string(),
        "MY_PROVIDER_API_KEY".to_string(),
        "my-custom-model".to_string(),
    )
    .with_working_dir(PathBuf::from("."))
    .with_timeout(120);

    println!("  代理类型: {}", custom_config.agent_type);
    println!("  命令: {}", custom_config.process.command);
    println!("  模型提供商: {:?}", custom_config.model_provider.as_ref().map(|p| &p.name));
    println!("  模型名称: {:?}", custom_config.model_name);
    if let Some(provider) = &custom_config.model_provider {
        println!("  Base URL: {}", provider.base_url);
        println!("  环境变量: {}", provider.env_key);
    }

    // 示例4: 手动设置模型提供商和模型名称
    println!("\n📋 示例4: 手动配置");
    let manual_config = AcpConfig::new("claude".to_string(), "python".to_string())
        .with_model_provider(acp_adapter::config::ModelProviderConfig::moonshot())
        .with_model_name("moonshot-v1-32k".to_string())  // 使用不同的模型
        .with_working_dir(PathBuf::from("."))
        .with_timeout(120);

    println!("  代理类型: {}", manual_config.agent_type);
    println!("  命令: {}", manual_config.process.command);
    println!("  模型提供商: {:?}", manual_config.model_provider.as_ref().map(|p| &p.name));
    println!("  模型名称: {:?}", manual_config.model_name);
    if let Some(provider) = &manual_config.model_provider {
        println!("  Base URL: {}", provider.base_url);
        println!("  环境变量: {}", provider.env_key);
    }

    // 示例5: 展示环境变量生成
    println!("\n📋 示例5: 环境变量生成");
    let env_config = AcpConfig::openai("claude".to_string(), "python".to_string());
    let env_vars = env_config.full_environment_with_provider();
    
    println!("  生成的环境变量:");
    for (key, value) in env_vars {
        if key.contains("API_KEY") || key.contains("BASE_URL") {
            println!("    {}: {}...", key, &value[..8.min(value.len())]);
        }
    }

    println!("\n🎯 使用建议:");
    println!("  1. 使用预定义配置: AcpConfig::glm(), AcpConfig::qwen() 等");
    println!("  2. 自定义模型: AcpConfig::custom_model()");
    println!("  3. 手动配置: with_model_provider() + with_model_name()");
    println!("  4. 设置环境变量: export GLM_AUTH_TOKEN=your_key");
    println!("  5. 使用完整环境变量: full_environment_with_provider()");

    Ok(())
}
