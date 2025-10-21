//! Docker 容器化 Agent 服务
//!
//! 通过 docker_manager 动态创建容器来运行 agent_server 服务，
//! 实现每个项目对应一个独立的 agent 容器

use crate::{
    model::ChatPrompt,
    proxy_agent::AcpConnectionInfo,
    CancelNotificationRequest,
};
use agent_client_protocol::{PromptRequest, SessionId};
use anyhow::{Context, Result};
use docker_manager::{DockerContainerConfig, DockerContainerInfo, DockerManager, ResourceLimits};
use reqwest::Client;
use serde_json::{json, Value};
use shared_types::ModelProviderConfig;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

/// Docker 容器 Agent 客户端
///
/// 通过 HTTP API 与容器内的 agent_server 通信
pub struct DockerContainerAgentClient {
    /// 容器信息
    container_info: DockerContainerInfo,
    /// HTTP 客户端
    http_client: Client,
    /// 容器内 agent_server 地址
    server_url: String,
}

// 注意：Docker 容器生命周期守卫现在使用内置的 AgentLifecycleGuard

/// 启动 Docker 容器化的 Agent 服务
pub async fn start_docker_container_agent_service(
    chat_prompt: ChatPrompt,
    model_provider: Option<ModelProviderConfig>,
    docker_manager: Arc<DockerManager>,
) -> Result<AcpConnectionInfo> {
    let project_id = chat_prompt.project_id.clone();
    let project_path = chat_prompt.project_path.clone();

    info!("启动 Docker 容器 Agent 服务，项目ID: {}", project_id);

    // 检查是否已存在该项目的容器
    if let Some(existing_container) = docker_manager.get_container_info(&project_id) {
        warn!("项目 {} 已存在容器 {}，将先停止", project_id, existing_container.container_name);
        if let Err(e) = docker_manager.stop_container(&project_id).await {
            error!("停止现有容器失败: {}", e);
            return Err(anyhow::anyhow!("无法停止现有容器: {}", e));
        }
        // 释放对应的端口
        if let Some(port_binding) = existing_container.port_bindings.values().next() {
            if let Ok(port) = port_binding.parse::<u16>() {
                crate::proxy_agent::port_manager::GLOBAL_PORT_MANAGER.release_port(port).await;
                info!("释放现有端口: {}", port);
            }
        }
    }

    // 分配端口（使用端口管理器避免冲突）
    let assigned_port = crate::proxy_agent::port_manager::GLOBAL_PORT_MANAGER.allocate_port().await
        .map_err(|e| anyhow::anyhow!("端口分配失败: {}", e))?;
    info!("为容器分配端口: {}", assigned_port);

    // 创建容器配置
    let container_config = create_docker_container_config(
        &project_id,
        &project_path,
        assigned_port,
        model_provider.as_ref(),
    )?;

    // 创建并启动容器
    let container_info = match docker_manager.create_container(container_config).await {
        Ok(info) => info,
        Err(e) => {
            // 创建失败，释放端口
            crate::proxy_agent::port_manager::GLOBAL_PORT_MANAGER.release_port(assigned_port).await;
            error!("创建容器失败: {}", e);
            return Err(anyhow::anyhow!("创建容器失败: {}", e));
        }
    };
    info!("容器已创建: {} (ID: {})", container_info.container_name, container_info.container_id);

    // 等待容器内 agent_server 启动
    let server_url = format!("http://localhost:{}", assigned_port);
    if let Err(e) = wait_for_agent_server_ready(&server_url).await {
        // 启动失败，清理容器和端口
        error!("容器内 agent_server 启动失败: {}", e);
        if let Err(stop_err) = docker_manager.stop_container_by_id(&container_info.container_id).await {
            error!("清理失败容器失败: {}", stop_err);
        }
        crate::proxy_agent::port_manager::GLOBAL_PORT_MANAGER.release_port(assigned_port).await;
        return Err(anyhow::anyhow!("容器内 agent_server 启动失败: {}", e));
    }

    // 创建聊天会话
    let session_id = create_chat_session(&server_url, &chat_prompt).await?;
    info!("已创建会话: {}", session_id);

    // 创建通信通道
    let (prompt_tx, prompt_rx) = mpsc::unbounded_channel::<PromptRequest>();
    let (cancel_tx, cancel_rx) = mpsc::unbounded_channel::<CancelNotificationRequest>();

    // 启动 HTTP 客户端处理任务
    let client = DockerContainerAgentClient {
        container_info: container_info.clone(),
        http_client: Client::new(),
        server_url: server_url.clone(),
    };

    // 启动后台任务处理 ACP 消息转发
    let project_id_clone = project_id.clone();
    let session_id_clone = session_id.clone();
    let server_url_clone = server_url.clone();
    let prompt_tx_clone = prompt_tx.clone();

    let task_handle = tokio::spawn(async move {
        if let Err(e) = run_acp_message_forwarding(
            &server_url_clone,
            &project_id_clone,
            &session_id_clone,
            prompt_rx,
            cancel_rx,
        ).await {
            error!("ACP 消息转发任务失败: {}", e);
        }
    });

    // 创建生命周期守卫
    let lifecycle_guard = crate::proxy_agent::agent_stop_handle::AgentLifecycleGuard::new_docker_container(
        project_id.clone(),
        session_id.clone(),
        Some(docker_manager.clone()),
        container_info.container_id.clone(),
        Some(assigned_port),
        tokio_util::sync::CancellationToken::new(),
    );

    Ok(AcpConnectionInfo {
        session_id,
        prompt_tx,
        cancel_tx,
        stop_handle: Some(Arc::new(lifecycle_guard)),
    })
}

/// 创建 Docker 容器配置
fn create_docker_container_config(
    project_id: &str,
    project_path: &std::path::Path,
    port: u16,
    model_provider: Option<&ModelProviderConfig>,
) -> Result<DockerContainerConfig> {
    let mut env_vars = HashMap::new();

    // 设置基本环境变量
    env_vars.insert("RUST_LOG".to_string(), "info".to_string());
    env_vars.insert("PROJECT_ID".to_string(), project_id.to_string());
    env_vars.insert("AGENT_TYPE".to_string(), "claude".to_string());

    // 设置模型提供商环境变量
    if let Some(provider) = model_provider {
        env_vars.insert("MODEL_PROVIDER_NAME".to_string(), provider.name.clone());
        env_vars.insert("MODEL_PROVIDER_API_KEY".to_string(), provider.api_key.clone());
        if !provider.base_url.is_empty() {
            env_vars.insert("MODEL_PROVIDER_BASE_URL".to_string(), provider.base_url.clone());
        }
        if !provider.default_model.is_empty() {
            env_vars.insert("MODEL_PROVIDER_DEFAULT_MODEL".to_string(), provider.default_model.clone());
        }
    }

    // 创建端口映射
    let mut port_bindings = HashMap::new();
    port_bindings.insert("8086/tcp".to_string(), port.to_string());

    // Docker 镜像内置 agent_server 二进制文件，无需额外挂载
    let extra_mounts = Vec::new(); // 不需要额外挂载 agent_server

    // 创建启动命令，直接使用镜像内置的 agent_server
    let command = vec![
        "/app/agent_server".to_string(),
        "--port".to_string(),
        "8086".to_string(),
        "--project-id".to_string(),
        project_id.to_string(),
        "--agent-type".to_string(),
        "claude".to_string(),
    ];

    Ok(DockerContainerConfig {
        project_id: project_id.to_string(),
        image: "registry.yichamao.com/rcoder:latest".to_string(),
        name_prefix: "rcoder-agent".to_string(),
        host_path: project_path.to_string_lossy().to_string(),
        container_path: "/app/workspace".to_string(),
        work_dir: "/app/workspace".to_string(),
        env_vars,
        port_bindings,
        network_mode: "host".to_string(), // 使用 host 网络模式便于通信
        auto_remove: true, // 容器停止后自动删除
        resource_limits: Some(ResourceLimits {
            memory_limit: Some(2 * 1024 * 1024 * 1024), // 2GB 内存
            cpu_limit: Some(2.0), // 2 核 CPU
            swap_limit: Some(4 * 1024 * 1024 * 1024), // 4GB 交换空间
        }),
        extra_mounts,
        command: Some(command),
    })
}


/// 等待容器内 agent_server 启动就绪
async fn wait_for_agent_server_ready(server_url: &str) -> Result<()> {
    let health_url = format!("{}/health", server_url);
    let client = Client::new();

    info!("等待 agent_server 启动: {}", health_url);

    // 最多等待 30 秒
    for attempt in 0..30 {
        match timeout(Duration::from_secs(1), client.get(&health_url).send()).await {
            Ok(Ok(response)) if response.status().is_success() => {
                info!("agent_server 已就绪");
                return Ok(());
            }
            Ok(_) => {
                debug!("agent_server 尚未就绪，等待中... ({}/30)", attempt + 1);
            }
            Err(_) => {
                debug!("连接超时，继续等待... ({}/30)", attempt + 1);
            }
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    Err(anyhow::anyhow!("等待 agent_server 启动超时"))
}

/// 创建聊天会话
async fn create_chat_session(server_url: &str, chat_prompt: &ChatPrompt) -> Result<SessionId> {
    let client = Client::new();
    let chat_url = format!("{}/chat", server_url);

    let mut request_body = json!({
        "prompt": chat_prompt.prompt,
        "project_id": chat_prompt.project_id,
    });

    if let Some(session_id) = &chat_prompt.session_id {
        request_body["session_id"] = json!(session_id);
    }

    if let Some(model_provider) = &chat_prompt.model_provider {
        request_body["model_provider"] = json!(model_provider);
    }

    let response = client.post(&chat_url)
        .json(&request_body)
        .send()
        .await
        .context("发送聊天请求失败")?;

    if !response.status().is_success() {
        return Err(anyhow::anyhow!("聊天请求失败: {}", response.status()));
    }

    let response_json: Value = response.json().await?;

    if let Some(data) = response_json.get("data") {
        if let Some(session_id) = data.get("session_id").and_then(|v| v.as_str()) {
            return Ok(SessionId(session_id.to_string().into()));
        }
    }

    Err(anyhow::anyhow!("无法从响应中获取会话ID"))
}

/// 运行 ACP 消息转发任务
async fn run_acp_message_forwarding(
    server_url: &str,
    project_id: &str,
    session_id: &SessionId,
    mut prompt_rx: mpsc::UnboundedReceiver<PromptRequest>,
    mut cancel_rx: mpsc::UnboundedReceiver<CancelNotificationRequest>,
) -> Result<()> {
    let client = Client::new();

    loop {
        tokio::select! {
            // 处理提示请求
            Some(prompt_request) = prompt_rx.recv() => {
                if let Err(e) = handle_prompt_request(&client, server_url, &prompt_request).await {
                    error!("处理提示请求失败: {}", e);
                }
            }

            // 处理取消请求
            Some(cancel_request) = cancel_rx.recv() => {
                if let Err(e) = handle_cancel_request(&client, server_url, cancel_request).await {
                    error!("处理取消请求失败: {}", e);
                }
            }

            // 任务结束
            else => {
                info!("ACP 消息转发任务结束");
                break;
            }
        }
    }

    Ok(())
}

/// 处理提示请求
async fn handle_prompt_request(
    client: &Client,
    server_url: &str,
    prompt_request: &PromptRequest,
) -> Result<()> {
    // 将 ACP PromptRequest 转换为 agent_server 的聊天格式
    let chat_url = format!("{}/chat", server_url);

    // 这里需要将 ACP 的 ContentBlock 转换为文本
    let prompt_text = extract_text_from_content_blocks(&prompt_request.prompt);

    let request_body = json!({
        "prompt": prompt_text,
        "session_id": prompt_request.session_id.0,
    });

    let response = client.post(&chat_url)
        .json(&request_body)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(anyhow::anyhow!("提示请求失败: {}", response.status()));
    }

    Ok(())
}

/// 处理取消请求
async fn handle_cancel_request(
    client: &Client,
    server_url: &str,
    cancel_request: CancelNotificationRequest,
) -> Result<()> {
    let cancel_url = format!("{}/agent/session/cancel", server_url);

    let request_body = json!({
        "session_id": cancel_request.cancel_notification.session_id.0,
        "request_id": "cancel_request",
    });

    let response = client.post(&cancel_url)
        .json(&request_body)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(anyhow::anyhow!("取消请求失败: {}", response.status()));
    }

    // 发送响应
    if let Err(_) = cancel_request.tx.send(crate::model::CancelNotificationResponse {
        success: true,
        message: Some("取消成功".to_string()),
    }) {
        warn!("发送取消响应失败，接收端已关闭");
    }

    Ok(())
}

/// 从 ContentBlock 中提取文本
fn extract_text_from_content_blocks(blocks: &[agent_client_protocol::ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| {
            if let agent_client_protocol::ContentBlock::Text(text_block) = block {
                Some(text_block.text.clone())
            } else {
                None
            }
        })
        .collect()
}

