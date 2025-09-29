//! Agent 自动选择示例
//!
//! 展示如何根据 ChatPrompt 中的 AgentType 字段自动选择使用 Codex 还是 Claude Code

use rcoder::model::{ChatPrompt, AgentType};
use rcoder::proxy_agent::agent_service::AcpAgentService;
use shared_types::ModelProviderConfig;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    // 示例 1: 使用 Claude Code 代理
    println!("=== 示例 1: 使用 Claude Code 代理 ===");
    let claude_prompt = ChatPrompt {
        project_id: "test_project_claude".to_string(),
        project_path: PathBuf::from("./project_workspace/test_project_claude"),
        session_id: None,
        prompt: "请帮我编写一个 Rust 的 Hello World 程序".to_string(),
        attachments: Vec::new(),
        agent_type: AgentType::Claude,
        request_id: None,
        context7_api_key: None,
        use_simple_prompt: false,
    };

    println!("选择的 Agent 类型: {:?}", claude_prompt.agent_type);
    println!("Agent 名称: {}", claude_prompt.agent_type.agent_type_name());
    println!("客户端能力: {:?}", claude_prompt.agent_type.get_client_capabilities());

    // 根据 AgentType 自动启动对应的代理服务
    // 注意：这里只是展示如何调用，实际使用时需要正确的环境配置
    /*
    match claude_prompt.agent_type.start_agent_service(
        claude_prompt.clone(),
        None, // 使用默认的模型提供商配置
    ).await {
        Ok(conn_info) => {
            println!("Claude 代理服务启动成功!");
            println!("会话 ID: {}", conn_info.session_id.0);
        }
        Err(e) => {
            println!("启动 Claude 代理服务失败: {}", e);
        }
    }
    */

    println!();

    // 示例 2: 使用 Codex 代理
    println!("=== 示例 2: 使用 Codex 代理 ===");
    let codex_prompt = ChatPrompt {
        project_id: "test_project_codex".to_string(),
        project_path: PathBuf::from("./project_workspace/test_project_codex"),
        session_id: None,
        prompt: "请帮我分析这个 Python 代码的性能问题".to_string(),
        attachments: Vec::new(),
        agent_type: AgentType::Codex,
        request_id: None,
        context7_api_key: None,
        use_simple_prompt: false,
    };

    println!("选择的 Agent 类型: {:?}", codex_prompt.agent_type);
    println!("Agent 名称: {}", codex_prompt.agent_type.agent_type_name());
    println!("客户端能力: {:?}", codex_prompt.agent_type.get_client_capabilities());

    // 根据AgentType自动启动对应的代理服务
    /*
    match codex_prompt.agent_type.start_agent_service(
        codex_prompt.clone(),
        None, // 使用默认的模型提供商配置
    ).await {
        Ok(conn_info) => {
            println!("Codex 代理服务启动成功!");
            println!("会话 ID: {}", conn_info.session_id.0);
        }
        Err(e) => {
            println!("启动 Codex 代理服务失败: {}", e);
        }
    }
    */

    println!();

    // 示例 3: 根据模型提供商配置自动选择 Agent 类型
    println!("=== 示例 3: 根据模型提供商配置自动选择 ===");

    // Anthropic 协议 -> 自动选择 Claude
    let anthropic_config = ModelProviderConfig {
        name: "anthropic".to_string(),
        base_url: "https://api.anthropic.com".to_string(),
        api_key: "sk-ant-api03-xxx".to_string(),
        default_model: "claude-3-sonnet-20240229".to_string(),
        requires_openai_auth: false,
    };

    let auto_agent_type = AgentType::from_model_provider(Some(&anthropic_config));
    println!("Anthropic 配置自动选择的 Agent: {:?}", auto_agent_type);

    // OpenAI 协议 -> 自动选择 Codex
    let openai_config = ModelProviderConfig {
        name: "openai".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        api_key: "sk-xxx".to_string(),
        default_model: "gpt-4".to_string(),
        requires_openai_auth: true,
    };

    let auto_agent_type = AgentType::from_model_provider(Some(&openai_config));
    println!("OpenAI 配置自动选择的 Agent: {:?}", auto_agent_type);

    // 无配置 -> 使用默认 Agent (Claude)
    let default_agent_type = AgentType::from_model_provider(None);
    println!("无配置时默认的 Agent: {:?}", default_agent_type);

    println!("\n=== 总结 ===");
    println!("1. ChatPrompt.agent_type 字段可以直接指定使用哪个代理");
    println!("2. AgentType.start_agent_service() 方法会根据类型自动调用对应的实现");
    println!("3. 也可以通过 ModelProviderConfig 自动选择合适的 Agent 类型");
    println!("4. 所有 Agent 都实现了统一的 AcpAgentService trait 接口");

    Ok(())
}