//! 模型配置示例
//! 
//! 展示如何配置不同的国内大模型来使用 Codex 或 Claude Code 工具

use ai_agents::{AgentConfig, AgentType, ModelProviderConfig, AgentManagerBuilder};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    tracing_subscriber::init();

    println!("=== AI 代理模型配置示例 ===\n");

    // 1. 使用 GLM-4.5 通过 Codex 代理
    println!("1. 配置 GLM-4.5 通过 Codex 代理使用:");
    let glm_codex_config = AgentConfig {
        agent_type: AgentType::Codex,
        model: "GLM-4.5".to_string(),
        provider: ModelProviderConfig::glm(),
        reasoning_effort: "high".to_string(),
        preferred_auth_method: "apikey".to_string(),
        ..AgentConfig::default()
    };
    
    println!("  - 代理类型: {}", glm_codex_config.agent_type);
    println!("  - 模型: {}", glm_codex_config.model);
    println!("  - 提供商: {}", glm_codex_config.provider.name);
    println!("  - Base URL: {}", glm_codex_config.provider.base_url);
    println!("  - 环境变量: {}", glm_codex_config.provider.env_key);
    
    // 生成环境变量
    let env_vars = glm_codex_config.provider.generate_env_vars(AgentType::Codex);
    println!("  - 生成的环境变量:");
    for (key, _) in &env_vars {
        println!("    {}=***", key);
    }
    println!();

    // 2. 使用 GLM-4.5 通过 Claude Code 代理
    println!("2. 配置 GLM-4.5 通过 Claude Code 代理使用:");
    let glm_claude_config = AgentConfig {
        agent_type: AgentType::Claude,
        model: "GLM-4.5".to_string(),
        provider: ModelProviderConfig::glm_anthropic(),
        reasoning_effort: "high".to_string(),
        preferred_auth_method: "apikey".to_string(),
        ..AgentConfig::default()
    };
    
    println!("  - 代理类型: {}", glm_claude_config.agent_type);
    println!("  - 模型: {}", glm_claude_config.model);
    println!("  - 提供商: {}", glm_claude_config.provider.name);
    println!("  - Base URL: {}", glm_claude_config.provider.base_url);
    println!("  - 环境变量: {}", glm_claude_config.provider.env_key);
    
    // 生成环境变量
    let env_vars = glm_claude_config.provider.generate_env_vars(AgentType::Claude);
    println!("  - 生成的环境变量:");
    for (key, _) in &env_vars {
        println!("    {}=***", key);
    }
    println!();

    // 3. 使用标准 OpenAI 配置
    println!("3. 配置标准 OpenAI GPT-4 通过 Codex 代理使用:");
    let openai_config = AgentConfig {
        agent_type: AgentType::Codex,
        model: "gpt-4".to_string(),
        provider: ModelProviderConfig::openai(),
        reasoning_effort: "medium".to_string(),
        preferred_auth_method: "apikey".to_string(),
        ..AgentConfig::default()
    };
    
    println!("  - 代理类型: {}", openai_config.agent_type);
    println!("  - 模型: {}", openai_config.model);
    println!("  - 提供商: {}", openai_config.provider.name);
    println!("  - Base URL: {}", openai_config.provider.base_url);
    println!("  - 环境变量: {}", openai_config.provider.env_key);
    println!();

    // 4. 使用标准 Claude 配置
    println!("4. 配置标准 Claude 通过 Claude Code 代理使用:");
    let claude_config = AgentConfig {
        agent_type: AgentType::Claude,
        model: "claude-3-5-sonnet-20241022".to_string(),
        provider: ModelProviderConfig::claude(),
        reasoning_effort: "high".to_string(),
        preferred_auth_method: "apikey".to_string(),
        ..AgentConfig::default()
    };
    
    println!("  - 代理类型: {}", claude_config.agent_type);
    println!("  - 模型: {}", claude_config.model);
    println!("  - 提供商: {}", claude_config.provider.name);
    println!("  - Base URL: {}", claude_config.provider.base_url);
    println!("  - 环境变量: {}", claude_config.provider.env_key);
    println!();

    // 5. 演示如何创建代理管理器
    println!("5. 创建代理管理器示例:");
    
    let (session_update_tx, _session_update_rx) = mpsc::unbounded_channel();
    let (client_tx, _client_rx) = mpsc::unbounded_channel();
    
    // 使用构建器模式创建管理器，优先使用 GLM-4.5
    let _manager_result = AgentManagerBuilder::new()
        .with_config(glm_codex_config.clone())
        .with_preferred_agents(vec![AgentType::Codex, AgentType::Claude])
        .build(session_update_tx, client_tx);
    
    match _manager_result {
        Ok(_manager) => {
            println!("  ✓ 代理管理器创建成功");
        }
        Err(e) => {
            println!("  ✗ 代理管理器创建失败: {}", e);
        }
    }

    println!("\n=== 配置说明 ===");
    println!("要使用这些配置，你需要设置相应的环境变量:");
    println!("- GLM: export GLM_AUTH_TOKEN=your-glm-token");
    println!("- OpenAI: export OPENAI_API_KEY=your-openai-key");
    println!("- Claude: export ANTHROPIC_API_KEY=your-claude-key");
    println!();
    println!("对应的 TOML 配置示例:");
    println!("GLM Codex 配置:");
    println!(r#"model_provider = "glm"
model = "GLM-4.5"
model_reasoning_effort = "high"
preferred_auth_method = "apikey"

[model_providers.glm]
name = "glm"
base_url = "https://open.bigmodel.cn/api/coding/paas/v4"
env_key = "GLM_AUTH_TOKEN"
requires_openai_auth = false"#);
    
    println!("\nGLM Claude Code 配置 (fish shell):");
    println!("set -Ux ANTHROPIC_BASE_URL https://open.bigmodel.cn/api/anthropic");
    println!("set -Ux ANTHROPIC_AUTH_TOKEN YOUR_GLM_TOKEN");
    println!("set -Ux ANTHROPIC_MODEL GLM-4.5");
    println!("set -Ux ANTHROPIC_SMALL_FAST_MODEL GLM-4.5-Air");

    Ok(())
}