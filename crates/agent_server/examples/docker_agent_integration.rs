//! Docker Agent 集成示例
//!
//! 展示如何使用 Docker Agent Manager 和 Agent Server

use anyhow::Result;
use docker_manager::docker_agent::{DockerAgentManager, AgentType};
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, Level};

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .init();

    info!("开始 Docker Agent 集成示例");

    // 创建 Docker Agent 管理器
    let agent_manager = DockerAgentManager::new().await?;
    let project_id = "example_project";
    let workspace_dir = "./project_workspace";

    info!("创建项目目录: {}/{}", workspace_dir, project_id);
    tokio::fs::create_dir_all(format!("{}/{}", workspace_dir, project_id)).await?;

    // 示例 1: 创建 Claude Agent
    info!("=== 示例 1: 创建 Claude Agent ===");
    create_claude_agent_example(&agent_manager, project_id, workspace_dir).await?;

    // 等待一段时间
    sleep(Duration::from_secs(5)).await;

    // 示例 2: 发送聊天请求
    info!("=== 示例 2: 发送聊天请求 ===");
    send_chat_request_example(&agent_manager, project_id).await?;

    // 等待一段时间
    sleep(Duration::from_secs(10)).await;

    // 示例 3: 检查 Agent 状态
    info!("=== 示例 3: 检查 Agent 状态 ===");
    check_agent_status_example(&agent_manager, project_id).await?;

    // 示例 4: 获取 Agent 日志
    info!("=== 示例 4: 获取 Agent 日志 ===");
    get_agent_logs_example(&agent_manager, project_id).await?;

    // 等待一段时间
    sleep(Duration::from_secs(5)).await;

    // 示例 5: 停止 Agent
    info!("=== 示例 5: 停止 Agent ===");
    stop_agent_example(&agent_manager, project_id).await?;

    // 清理
    cleanup_example(workspace_dir).await?;

    info!("Docker Agent 集成示例完成");
    Ok(())
}

/// 创建 Claude Agent 示例
async fn create_claude_agent_example(
    agent_manager: &DockerAgentManager,
    project_id: &str,
    workspace_dir: &str,
) -> Result<()> {
    info!("创建 Claude Agent，项目ID: {}", project_id);

    // 配置模型提供商
    let mut model_provider = HashMap::new();
    model_provider.insert("name".to_string(), "anthropic".to_string());
    model_provider.insert("base_url".to_string(), "https://api.anthropic.com".to_string());
    model_provider.insert("api_key".to_string(), "your_anthropic_api_key".to_string());
    model_provider.insert("default_model".to_string(), "claude-3-5-sonnet-20241022".to_string());
    model_provider.insert("requires_openai_auth".to_string(), "false".to_string());
    model_provider.insert("CLAUDE_CODE_ARGS".to_string(), "--dangerously-skip-permissions".to_string());

    // 创建 Docker Agent
    let agent_info = agent_manager
        .create_docker_agent(
            project_id,
            AgentType::Claude,
            workspace_dir,
            Some(model_provider),
        )
        .await?;

    info!("Claude Agent 创建成功:");
    info!("  容器ID: {}", agent_info.container_info.container_id);
    info!("  容器名称: {}", agent_info.container_info.container_name);
    info!("  Agent 类型: {:?}", agent_info.agent_type);
    info!("  Server URL: {}", agent_info.server_url);

    Ok(())
}

/// 发送聊天请求示例
async fn send_chat_request_example(
    agent_manager: &DockerAgentManager,
    project_id: &str,
) -> Result<()> {
    info!("发送聊天请求到项目: {}", project_id);

    let chat_request = json!({
        "prompt": "请帮我创建一个简单的 Rust Hello World 程序",
        "project_id": project_id,
        "session_id": None,
        "model_provider": {
            "name": "anthropic",
            "base_url": "https://api.anthropic.com",
            "api_key": "your_anthropic_api_key",
            "default_model": "claude-3-5-sonnet-20241022",
            "requires_openai_auth": false
        },
        "attachments": [],
        "data_source_attachments": [],
        "request_id": Some("example_request_001".to_string())
    });

    match agent_manager.send_chat_request(project_id, chat_request).await {
        Ok(response) => {
            info!("聊天请求已发送:");
            info!("  会话ID: {}", response.session_id);
            info!("  请求ID: {}", response.request_id);
            info!("  状态: {}", response.status);
            if let Some(content) = response.content {
                info!("  内容: {}", content);
            }
            if let Some(error) = response.error {
                info!("  错误: {}", error);
            }
        }
        Err(e) => {
            info!("聊天请求失败: {}", e);
        }
    }

    Ok(())
}

/// 检查 Agent 状态示例
async fn check_agent_status_example(
    agent_manager: &DockerAgentManager,
    project_id: &str,
) -> Result<()> {
    info!("检查 Agent 状态，项目ID: {}", project_id);

    if let Some(agent_info) = agent_manager.get_docker_agent(project_id) {
        info!("Agent 信息:");
        info!("  项目ID: {}", agent_info.project_id);
        info!("  Agent 类型: {:?}", agent_info.agent_type);
        info!("  创建时间: {}", agent_info.created_at);
        info!("  Server URL: {}", agent_info.server_url);
    }

    // 检查容器状态
    match agent_manager.check_agent_status(project_id).await {
        Ok(Some(status)) => {
            info!("容器状态: {:?}", status);
        }
        Ok(None) => {
            info!("容器不存在");
        }
        Err(e) => {
            info!("检查状态失败: {}", e);
        }
    }

    // 列出所有 Agent
    let agents = agent_manager.list_docker_agents();
    info!("当前活跃的 Docker Agent 数量: {}", agents.len());
    for agent in agents {
        info!("  - 项目: {}, 类型: {:?}, 状态: 运行中", agent.project_id, agent.agent_type);
    }

    Ok(())
}

/// 获取 Agent 日志示例
async fn get_agent_logs_example(
    agent_manager: &DockerAgentManager,
    project_id: &str,
) -> Result<()> {
    info!("获取 Agent 日志，项目ID: {}", project_id);

    match agent_manager.get_agent_logs(project_id, 20).await {
        Ok(logs) => {
            info!("Agent 日志 (最后20行):");
            for (i, line) in logs.lines().enumerate() {
                info!("  [{}] {}", i + 1, line);
            }
        }
        Err(e) => {
            info!("获取日志失败: {}", e);
        }
    }

    Ok(())
}

/// 停止 Agent 示例
async fn stop_agent_example(
    agent_manager: &DockerAgentManager,
    project_id: &str,
) -> Result<()> {
    info!("停止 Agent，项目ID: {}", project_id);

    match agent_manager.stop_docker_agent(project_id).await {
        Ok(_) => {
            info!("Agent 已成功停止");
        }
        Err(e) => {
            info!("停止 Agent 失败: {}", e);
        }
    }

    Ok(())
}

/// 清理示例
async fn cleanup_example(workspace_dir: &str) -> Result<()> {
    info!("清理示例资源");

    // 停止所有 Agent
    let agent_manager = DockerAgentManager::new().await?;
    if let Err(e) = agent_manager.stop_all_docker_agents().await {
        info!("停止所有 Agent 失败: {}", e);
    }

    // 清理项目目录
    let project_path = format!("{}/example_project", workspace_dir);
    if std::path::Path::new(&project_path).exists() {
        if let Err(e) = tokio::fs::remove_dir_all(&project_path).await {
            info!("删除项目目录失败: {}", e);
        } else {
            info!("已删除项目目录: {}", project_path);
        }
    }

    Ok(())
}

/// 创建 Codex Agent 示例
async fn create_codex_agent_example(
    agent_manager: &DockerAgentManager,
    project_id: &str,
    workspace_dir: &str,
) -> Result<()> {
    info!("创建 Codex Agent，项目ID: {}", project_id);

    // 配置 OpenAI 模型提供商
    let mut model_provider = HashMap::new();
    model_provider.insert("name".to_string(), "openai".to_string());
    model_provider.insert("base_url".to_string(), "https://api.openai.com/v1".to_string());
    model_provider.insert("api_key".to_string(), "your_openai_api_key".to_string());
    model_provider.insert("default_model".to_string(), "gpt-4".to_string());
    model_provider.insert("requires_openai_auth".to_string(), "true".to_string());

    // 创建 Docker Agent
    let agent_info = agent_manager
        .create_docker_agent(
            project_id,
            AgentType::Codex,
            workspace_dir,
            Some(model_provider),
        )
        .await?;

    info!("Codex Agent 创建成功:");
    info!("  容器ID: {}", agent_info.container_info.container_id);
    info!("  容器名称: {}", agent_info.container_info.container_name);
    info!("  Agent 类型: {:?}", agent_info.agent_type);
    info!("  Server URL: {}", agent_info.server_url);

    Ok(())
}