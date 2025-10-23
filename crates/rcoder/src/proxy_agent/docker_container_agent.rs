//! Docker 容器化 Agent 服务
//!
//! 通过 docker_manager 动态创建容器来运行 agent_runner 服务，
//! 实现每个项目对应一个独立的 agent 容器

use crate::{CancelNotificationRequest, proxy_agent::AcpConnectionInfo};
use agent_client_protocol::{PromptRequest, SessionId};
use anyhow::{Context, Result};
use docker_manager::{DockerContainerConfig, DockerContainerInfo, DockerManager, ResourceLimits};
use reqwest::Client;
use serde_json::{Value, json};
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
    /// 容器内 agent_runner 地址
    server_url: String,
}

// 注意：Docker 容器生命周期守卫现在使用内置的 AgentLifecycleGuard

/// 启动 Docker 容器化的 Agent 服务,project_path是容器里的相对路径
/// 专注于创建容器并返回基本的容器信息
pub async fn start_docker_container_agent_service(
    project_id: String,
    project_path: String,
    docker_manager: Arc<DockerManager>,
) -> Result<(DockerContainerInfo, String)> {
    info!(
        "启动 Docker 容器 Agent 服务（使用 agent_runner），项目ID: {}",
        project_id
    );
    info!(
        "📁 [DOCKER_AGENT] 项目工作目录: project_id={}, project_path={:?}",
        project_id, project_path
    );

    // 检查是否已存在该项目的容器
    if let Some(existing_container) = docker_manager.get_container_info(&project_id) {
        warn!(
            "项目 {} 已存在容器 {}，将先停止",
            project_id, existing_container.container_name
        );
        if let Err(e) = docker_manager.stop_container(&project_id).await {
            error!("停止现有容器失败: {}", e);
            return Err(anyhow::anyhow!("无法停止现有容器: {}", e));
        }
        // 释放对应的端口（如果存在端口映射）
        if let Some(port_binding) = existing_container.port_bindings.values().next() {
            if let Ok(port) = port_binding.parse::<u16>() {
                crate::proxy_agent::port_manager::GLOBAL_PORT_MANAGER
                    .release_port(port)
                    .await;
                info!("释放现有端口: {}", port);
            }
        }
    }

    // 🎯 优化：不再需要宿主机端口映射，使用内部网络通信
    info!("使用容器内部网络通信，无需宿主机端口映射");

    // 创建容器配置（无需端口映射）
    let container_config = create_docker_container_config(&project_id, &project_path).await?;

    // 创建并启动容器
    let container_info = match docker_manager.create_container(container_config).await {
        Ok(info) => info,
        Err(e) => {
            // 🎯 优化：使用内部网络，无需端口分配和释放
            error!("创建容器失败: {}", e);
            return Err(anyhow::anyhow!("创建容器失败: {}", e));
        }
    };
    info!(
        "容器已创建: {} (ID: {})",
        container_info.container_name, container_info.container_id
    );

    // 等待容器内 agent_runner 启动
    // 🎯 优化：使用容器内部IP，无需宿主机端口映射
    let server_url = get_container_ip(&docker_manager, &container_info.container_id).await?;

    if let Err(e) = wait_for_agent_server_ready(&server_url).await {
        // 启动失败，清理容器（无需端口管理）
        error!("容器内 agent_runner 启动失败: {}", e);
        if let Err(stop_err) = docker_manager
            .stop_container_by_id(&container_info.container_id)
            .await
        {
            error!("清理失败容器失败: {}", stop_err);
        }
        return Err(anyhow::anyhow!("容器内 agent_runner 启动失败: {}", e));
    }

    info!("✅ 容器服务启动成功: {}", server_url);
    Ok((container_info, server_url))
}

/// 创建 Docker 容器配置（内部网络通信，无需端口映射）
async fn create_docker_container_config(
    project_id: &str,
    project_path: &str,
) -> Result<DockerContainerConfig> {
    let mut env_vars = HashMap::new();

    // 设置基本环境变量
    env_vars.insert("RUST_LOG".to_string(), "info".to_string());
    env_vars.insert("PROJECT_ID".to_string(), project_id.to_string());
    env_vars.insert("AGENT_TYPE".to_string(), "claude".to_string());

    // 🔄 关键：将容器内路径转换为宿主机路径（自动检测模式）
    // 先将路径标准化，处理相对路径情况
    let normalized_path = std::path::PathBuf::from(project_path);
    let host_project_path = if normalized_path.is_relative() {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("/"))
            .join(&normalized_path)
    } else {
        normalized_path
    };

    let host_project_path = crate::utils::resolve_container_path_to_host(&host_project_path)
        .await
        .context("自动检测宿主机路径失败，请检查 Docker socket 挂载和权限")?;
    info!(
        "✅ 路径自动检测成功: 容器内 {:?} -> 宿主机 {:?}",
        project_path, host_project_path
    );

    // 🎯 优化：无需端口映射，使用内部网络通信
    let port_bindings = HashMap::new(); // 空的端口映射

    // Docker 镜像内置 agent_runner 二进制文件，无需额外挂载
    let extra_mounts = Vec::new(); // 不需要额外挂载 agent_runner

    // 创建启动命令，直接使用镜像内置的 agent_runner
    let command = vec![
        "/app/bin/agent_runner".to_string(),
        "--port".to_string(),
        "8086".to_string(),
    ];

    Ok(DockerContainerConfig {
        project_id: project_id.to_string(),
        image: docker_manager::default_docker_image(),
        name_prefix: "rcoder-agent".to_string(),
        host_path: host_project_path.to_string_lossy().to_string(), // 🎯 使用宿主机绝对路径
        container_path: "/app/workspace".to_string(),
        work_dir: "/app/workspace".to_string(),
        env_vars,
        port_bindings,                      // 空的端口映射
        network_mode: "bridge".to_string(), // 使用 bridge 网络模式，但不暴露端口
        auto_remove: true,                  // 容器停止后自动删除
        resource_limits: Some(ResourceLimits {
            memory_limit: Some(2 * 1024 * 1024 * 1024), // 2GB 内存
            cpu_limit: Some(2.0),                       // 2 核 CPU
            swap_limit: Some(4 * 1024 * 1024 * 1024),   // 4GB 交换空间
        }),
        extra_mounts,
        command: Some(command),
        entrypoint: Some(Vec::new()), // 覆盖默认入口点，直接运行命令
    })
}

/// 等待容器内 agent_runner 启动就绪
async fn wait_for_agent_server_ready(server_url: &str) -> Result<()> {
    let health_url = format!("{}/health", server_url);
    let client = Client::new();

    info!("等待 agent_runner 启动: {}", health_url);

    // 最多等待 30 秒
    for attempt in 0..30 {
        match timeout(Duration::from_secs(1), client.get(&health_url).send()).await {
            Ok(Ok(response)) if response.status().is_success() => {
                info!("agent_runner 已就绪");
                return Ok(());
            }
            Ok(_) => {
                debug!("agent_runner 尚未就绪，等待中... ({}/30)", attempt + 1);
            }
            Err(_) => {
                debug!("连接超时，继续等待... ({}/30)", attempt + 1);
            }
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    Err(anyhow::anyhow!("等待 agent_runner 启动超时"))
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
    // 将 ACP PromptRequest 转换为 agent_runner 的聊天格式
    let chat_url = format!("{}/chat", server_url);

    // 这里需要将 ACP 的 ContentBlock 转换为文本
    let prompt_text = extract_text_from_content_blocks(&prompt_request.prompt);

    let request_body = json!({
        "prompt": prompt_text,
        "session_id": prompt_request.session_id.0,
    });

    let response = client.post(&chat_url).json(&request_body).send().await?;

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

    let response = client.post(&cancel_url).json(&request_body).send().await?;

    if !response.status().is_success() {
        return Err(anyhow::anyhow!("取消请求失败: {}", response.status()));
    }

    // 发送响应
    if let Err(_) = cancel_request
        .tx
        .send(shared_types::CancelNotificationResponse {
            success: true,
            message: Some("取消成功".to_string()),
        })
    {
        warn!("发送取消响应失败，接收端已关闭");
    }

    Ok(())
}

/// 获取容器在 agent-network 中的 IP 地址（无宿主机端口映射）
pub async fn get_container_ip(
    docker_manager: &DockerManager,
    container_id: &str,
) -> Result<String> {
    // 等待容器网络配置完成
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

    // 通过 DockerManager 获取容器的网络信息
    let network_ips = docker_manager
        .get_container_network_info(container_id)
        .await
        .map_err(|e| anyhow::anyhow!("获取容器网络信息失败: {}", e))?;

    // 直接查找 agent-network 的 IP 地址
    let network_name = docker_manager.get_rcoder_network_name();
    if let Some(ip_address) = network_ips.get(network_name) {
        let server_url = format!("http://{}:8086", ip_address);
        info!("✅ 获取容器 IP 地址: {} -> {}", container_id, ip_address);
        Ok(server_url)
    } else {
        Err(anyhow::anyhow!(
            "容器 {} 未连接到 agent-network: {}",
            container_id,
            network_name
        ))
    }
}

/// 🎯 优化：使用容器名称通过 Docker 内置 DNS 解析
/// 无需获取容器 IP，直接使用容器名称
pub async fn get_container_server_url(container_name: &str) -> Result<String> {
    // 直接使用容器名称，Docker 内置 DNS 会自动解析
    let server_url = format!("http://{}:8086", container_name);
    info!(
        "✅ 使用容器名称 DNS 解析: {} -> {}",
        container_name, server_url
    );
    Ok(server_url)
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
