#!/usr/bin/env cargo

//! AI 代理使用示例
//!
//! 演示如何使用统一的 AI 代理管理器来透明地访问不同的 AI 工具（Claude、Codex）

use ai_agents::{AgentConfig, AgentManager, AgentManagerBuilder, AgentType, ManagedAgent, AgentClientOp};
use agent_client_protocol::{
    Agent, InitializeRequest, PromptRequest, ContentBlock, TextContent,
    AuthenticateRequest, AuthMethodId, SessionNotification,
};
use tokio::sync::mpsc;
use std::sync::Arc;
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志
    tracing_subscriber::fmt::init();

    // 创建通信通道
    let (session_tx, mut session_rx) = mpsc::unbounded_channel();
    let (client_tx, mut client_rx) = mpsc::unbounded_channel::<AgentClientOp>();

    // 配置代理
    let config = AgentConfig {
        agent_type: AgentType::Codex, // 使用 Codex 代理来调用 GLM
        cwd: std::env::current_dir()?,
        home_dir: dirs::home_dir()
            .unwrap_or_default()
            .join(".ai-agents"),
        model: "GLM-4.5".to_string(),  // 使用 GLM-4.5 模型
        env_vars: {
            let mut env = std::collections::HashMap::new();
            // 设置 GLM 认证 token
            if let Ok(token) = std::env::var("GLM_AUTH_TOKEN") {
                env.insert("GLM_AUTH_TOKEN".to_string(), token);
            }
            env
        },
    };

    // 使用构建器创建代理管理器
    let manager = AgentManagerBuilder::new()
        .with_config(config)
        .with_preferred_agents(vec![AgentType::Codex, AgentType::Claude])  // 优先使用 Codex 支持 GLM
        .build(session_tx, client_tx)?;

    let manager = Arc::new(tokio::sync::RwLock::new(manager));

    // 创建托管代理
    let managed_agent = ManagedAgent::new(manager.clone());

    println!("🚀 初始化 AI 代理管理器...");

    // 初始化代理
    let init_response = managed_agent.initialize(InitializeRequest {
        protocol_version: agent_client_protocol::V1,
        client_capabilities: agent_client_protocol::ClientCapabilities {
            fs: agent_client_protocol::FileSystemCapability {
                read_text_file: true,
                write_text_file: true,
                meta: None,
            },
            terminal: true,
            meta: None,
        },
        meta: None,
    }).await?;

    println!("✅ 代理初始化成功，协议版本: {:?}", init_response.protocol_version);
    println!("📋 支持的认证方法: {:?}", init_response.auth_methods);

    // 检查当前使用的代理
    {
        let manager_read = manager.read().await;
        if let Some(agent_type) = manager_read.current_agent_type() {
            println!("🤖 当前使用的代理: {}", agent_type);
        }
        println!("🔍 可用的代理: {:?}", manager_read.available_agents());
    }

    // 尝试认证
    for auth_method in &init_response.auth_methods {
        println!("🔐 尝试认证方法: {}", auth_method.name);
        
        match managed_agent.authenticate(AuthenticateRequest {
            method_id: auth_method.id.clone(),
            meta: None,
        }).await {
            Ok(_) => {
                println!("✅ 认证成功: {}", auth_method.name);
                break;
            }
            Err(e) => {
                println!("❌ 认证失败: {} - {}", auth_method.name, e);
            }
        }
    }

    // 创建新会话
    let session_response = managed_agent.new_session(agent_client_protocol::NewSessionRequest {
        meta: None,
    }).await?;

    println!("🔗 创建会话成功: {:?}", session_response.session_id);

    // 发送测试提示
    let prompt = vec![
        ContentBlock::Text(TextContent {
            text: "Hello! Please introduce yourself and tell me what you can do.".to_string(),
            annotations: None,
            meta: None,
        })
    ];

    println!("💬 发送提示...");
    
    let prompt_response = managed_agent.prompt(PromptRequest {
        session_id: session_response.session_id.clone(),
        prompt,
        meta: None,
    }).await?;

    println!("✅ 提示处理完成，停止原因: {:?}", prompt_response.stop_reason);

    // 演示代理切换
    {
        let mut manager_write = manager.write().await;
        let available = manager_write.available_agents();
        
        if available.len() > 1 {
            println!("\n🔄 演示代理切换...");
            
            for agent_type in available {
                match manager_write.switch_agent(agent_type) {
                    Ok(_) => println!("✅ 切换到 {} 代理", agent_type),
                    Err(e) => println!("❌ 切换到 {} 代理失败: {}", agent_type, e),
                }
            }
        }
    }

    // 处理会话更新
    tokio::spawn(async move {
        while let Some((notification, response_tx)) = session_rx.recv().await {
            println!("📨 收到会话通知: {:?}", notification.update);
            let _ = response_tx.send(());
        }
    });

    // 处理客户端操作
    tokio::spawn(async move {
        while let Some(op) = client_rx.recv().await {
            println!("🔧 收到客户端操作: {:?}", op);
        }
    });

    println!("\n✨ AI 代理管理器示例完成!");
    
    // 等待一下让异步任务完成
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    Ok(())
}

/// 演示如何手动注册和管理代理
async fn manual_agent_management_example() -> Result<()> {
    let (session_tx, _session_rx) = mpsc::unbounded_channel();
    let (client_tx, _client_rx) = mpsc::unbounded_channel();

    let mut manager = AgentManager::new(session_tx);

    // 检查可用的代理
    println!("🔍 检查代理可用性:");
    for agent_type in [AgentType::Claude, AgentType::Codex] {
        let available = AgentManager::is_agent_available(agent_type);
        println!("  {} {}: {}", 
            if available { "✅" } else { "❌" },
            agent_type,
            if available { "可用" } else { "不可用" }
        );

        if available {
            println!("    认证方法: {:?}", AgentManager::get_auth_methods(agent_type));
        }
    }

    // 手动注册 Claude 代理
    if AgentManager::is_agent_available(AgentType::Claude) {
        let config = AgentConfig {
            agent_type: AgentType::Claude,
            cwd: std::env::current_dir()?,
            home_dir: dirs::home_dir().unwrap_or_default().join(".claude"),
            model: "claude-3-5-sonnet-20241022".to_string(),
            env_vars: std::collections::HashMap::new(),
        };

        manager.register_agent(AgentType::Claude, config, client_tx.clone())?;
        println!("✅ 手动注册 Claude 代理成功");
    }

    // 自动注册其他代理
    let base_config = AgentConfig::default();
    let registered = manager.auto_register_agents(base_config, client_tx)?;
    println!("🤖 自动注册的代理: {:?}", registered);

    Ok(())
}

/// 演示如何配置特定的代理
fn agent_configuration_examples() {
    println!("\n⚙️  代理配置示例:");

    // Claude 配置
    let claude_config = AgentConfig {
        agent_type: AgentType::Claude,
        cwd: std::path::PathBuf::from("/path/to/project"),
        home_dir: std::path::PathBuf::from("/home/user/.claude"),
        model: "claude-3-5-sonnet-20241022".to_string(),
        env_vars: {
            let mut env = std::collections::HashMap::new();
            env.insert("CLAUDE_API_KEY".to_string(), "your-api-key".to_string());
            env
        },
    };
    println!("Claude 配置: {:?}", claude_config);

    // Codex 配置（支持 GLM-4.5）
    let codex_config = AgentConfig {
        agent_type: AgentType::Codex,
        cwd: std::path::PathBuf::from("/path/to/project"),
        home_dir: std::path::PathBuf::from("/home/user/.codex"),
        model: "GLM-4.5".to_string(),  // 使用 GLM-4.5 模型
        env_vars: {
            let mut env = std::collections::HashMap::new();
            env.insert("GLM_AUTH_TOKEN".to_string(), "your-glm-token".to_string());
            env
        },
    };
    println!("Codex 配置（GLM-4.5）: {:?}", codex_config);
}