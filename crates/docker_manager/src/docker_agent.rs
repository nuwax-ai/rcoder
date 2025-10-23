//! Docker Agent 集成模块
//!
//! 这个模块提供了与 Docker 容器内 Agent Server 通信的功能

use super::{
    DockerContainerConfig, DockerContainerInfo, DockerManager, DockerManagerConfig, DockerResult,
    MountPoint,
};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Docker Agent 管理器 - 专门用于管理容器内的 Agent Server
pub struct DockerAgentManager {
    /// Docker 管理器
    docker_manager: Arc<DockerManager>,
    /// 活跃的 Docker Agent 映射
    agents: Arc<DashMap<String, DockerAgentInfo>>,
}

/// Docker Agent 信息
#[derive(Clone)]
pub struct DockerAgentInfo {
    /// 项目 ID
    pub project_id: String,
    /// 容器信息
    pub container_info: DockerContainerInfo,
    /// Agent 类型
    pub agent_type: AgentType,
    /// Agent Server 地址
    pub server_url: String,
    /// 创建时间
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Agent 类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentType {
    Claude,
    Codex,
}

impl DockerAgentManager {
    /// 创建新的 Docker Agent 管理器
    pub async fn new() -> Result<Self> {
        let config = DockerManagerConfig::default();
        let docker_manager = Arc::new(DockerManager::new(config).await?);

        info!("Docker Agent 管理器初始化成功");

        Ok(Self {
            docker_manager,
            agents: Arc::new(DashMap::new()),
        })
    }

    /// 使用自定义配置创建 Docker Agent 管理器
    pub async fn with_config(config: DockerManagerConfig) -> Result<Self> {
        let docker_manager = Arc::new(DockerManager::new(config).await?);

        info!("Docker Agent 管理器初始化成功（自定义配置）");

        Ok(Self {
            docker_manager,
            agents: Arc::new(DashMap::new()),
        })
    }

    /// 创建并启动 Docker Agent
    pub async fn create_docker_agent(
        &self,
        project_id: &str,
        agent_type: AgentType,
        host_workspace_dir: &str,
        model_provider: Option<HashMap<String, String>>,
    ) -> Result<DockerAgentInfo> {
        info!(
            "开始创建 Docker Agent，项目ID: {}, Agent类型: {:?}",
            project_id, agent_type
        );

        // 检查是否已存在该项目的 Docker Agent
        if let Some(existing) = self.agents.get(project_id) {
            warn!("项目 {} 已存在 Docker Agent，将先停止", project_id);
            self.stop_docker_agent(project_id).await?;
        }

        // 创建 Docker 容器配置
        let config = self.create_agent_container_config(
            project_id,
            agent_type,
            host_workspace_dir,
            model_provider,
        )?;

        // 确保项目目录存在
        if !std::path::Path::new(&config.host_path).exists() {
            tokio::fs::create_dir_all(&config.host_path).await?;
            info!("创建项目目录: {}", config.host_path);
        }

        // 创建并启动容器
        let container_info = self.docker_manager.create_container(config).await?;

        // 等待容器内的 Agent Server 启动
        let server_url = format!("http://localhost:8086");
        self.wait_for_agent_ready(&server_url, 30).await?;

        // 创建 Docker Agent 信息
        let docker_agent_info = DockerAgentInfo {
            project_id: project_id.to_string(),
            container_info: container_info.clone(),
            agent_type,
            server_url,
            created_at: chrono::Utc::now(),
        };

        // 保存到映射
        self.agents
            .insert(project_id.to_string(), docker_agent_info.clone());

        info!("Docker Agent 创建成功: {}", container_info.container_name);

        Ok(docker_agent_info)
    }

    /// 停止 Docker Agent
    pub async fn stop_docker_agent(&self, project_id: &str) -> Result<()> {
        info!("停止 Docker Agent，项目ID: {}", project_id);

        // 从 Docker 管理器停止容器
        if let Err(e) = self.docker_manager.stop_container(project_id).await {
            error!("停止 Docker 容器失败: {}", e);
        }

        // 从映射中移除
        self.agents.remove(project_id);

        info!("Docker Agent 已停止，项目ID: {}", project_id);
        Ok(())
    }

    /// 获取 Docker Agent 信息
    pub fn get_docker_agent(&self, project_id: &str) -> Option<DockerAgentInfo> {
        self.agents.get(project_id).map(|agent| agent.clone())
    }

    /// 列出所有 Docker Agent
    pub fn list_docker_agents(&self) -> Vec<DockerAgentInfo> {
        self.agents
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// 停止所有 Docker Agent
    pub async fn stop_all_docker_agents(&self) -> Result<()> {
        info!("开始停止所有 Docker Agent");

        let project_ids: Vec<String> = self
            .agents
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        for project_id in project_ids {
            if let Err(e) = self.stop_docker_agent(&project_id).await {
                error!(
                    "停止 Docker Agent 失败，项目ID: {}, 错误: {}",
                    project_id, e
                );
            }
        }

        info!("所有 Docker Agent 已停止");
        Ok(())
    }

    /// 检查 Docker Agent 状态
    pub async fn check_agent_status(&self, project_id: &str) -> Result<Option<ContainerStatus>> {
        self.docker_manager
            .update_container_status(project_id)
            .await
            .map_err(|e| anyhow::anyhow!("更新容器状态失败: {}", e))
    }

    /// 获取 Docker Agent 日志
    pub async fn get_agent_logs(&self, project_id: &str, lines: i64) -> Result<String> {
        self.docker_manager
            .get_container_logs(project_id, lines)
            .await
            .map_err(|e| anyhow::anyhow!("获取容器日志失败: {}", e))
    }

    /// 发送聊天请求到 Docker Agent
    pub async fn send_chat_request(
        &self,
        project_id: &str,
        request: ChatRequest,
    ) -> Result<ChatResponse> {
        let agent_info = self
            .get_docker_agent(project_id)
            .ok_or_else(|| anyhow::anyhow!("项目 {} 没有对应的 Docker Agent", project_id))?;

        // 发送 HTTP 请求到容器内的 Agent Server
        let client = reqwest::Client::new();
        let url = format!("{}/chat", agent_info.server_url);

        debug!("发送聊天请求到: {}", url);

        let response = client
            .post(&url)
            .json(&request)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("发送请求失败: {}", e))?;

        if response.status().is_success() {
            let chat_response: ChatResponse = response
                .json()
                .await
                .map_err(|e| anyhow::anyhow!("解析响应失败: {}", e))?;
            Ok(chat_response)
        } else {
            let error_text = response.text().await.unwrap_or_default();
            Err(anyhow::anyhow!(
                "请求失败: {} - {}",
                response.status(),
                error_text
            ))
        }
    }

    /// 等待 Agent Server 准备就绪
    async fn wait_for_agent_ready(&self, server_url: &str, timeout_seconds: u64) -> Result<()> {
        info!("等待 Agent Server 准备就绪: {}", server_url);

        let client = reqwest::Client::new();
        let health_url = format!("{}/health", server_url);
        let start_time = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(timeout_seconds);

        while start_time.elapsed() < timeout {
            match client.get(&health_url).send().await {
                Ok(response) if response.status().is_success() => {
                    info!("Agent Server 已准备就绪");
                    return Ok(());
                }
                Ok(_) => {
                    debug!("Agent Server 尚未准备就绪，等待中...");
                }
                Err(e) => {
                    debug!("健康检查失败: {}", e);
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        Err(anyhow::anyhow!("等待 Agent Server 准备就绪超时"))
    }

    /// 创建 Agent 容器配置
    fn create_agent_container_config(
        &self,
        project_id: &str,
        agent_type: AgentType,
        host_workspace_dir: &str,
        model_provider: Option<HashMap<String, String>>,
    ) -> DockerResult<DockerContainerConfig> {
        let mut env_vars = HashMap::new();

        // 基础环境变量
        env_vars.insert("RUST_LOG".to_string(), "info".to_string());
        env_vars.insert("TZ".to_string(), "Asia/Shanghai".to_string());
        env_vars.insert("PROJECT_ID".to_string(), project_id.to_string());
        env_vars.insert("AGENT_SERVER_PORT".to_string(), "8086".to_string());

        // Agent 类型特定环境变量
        match agent_type {
            AgentType::Claude => {
                env_vars.insert("AGENT_TYPE".to_string(), "claude".to_string());

                if let Some(ref provider) = model_provider {
                    for (key, value) in provider {
                        if key.starts_with("ANTHROPIC_") || key == "CLAUDE_CODE_ARGS" {
                            env_vars.insert(key.clone(), value.clone());
                        }
                    }
                }
            }
            AgentType::Codex => {
                env_vars.insert("AGENT_TYPE".to_string(), "codex".to_string());

                if let Some(ref provider) = model_provider {
                    for (key, value) in provider {
                        if key.starts_with("OPENAI_") || key.starts_with("CODEX_") {
                            env_vars.insert(key.clone(), value.clone());
                        }
                    }
                }
            }
        }

        // 创建容器配置
        let config = DockerContainerConfig {
            project_id: project_id.to_string(),
            image: crate::default_docker_image(),
            name_prefix: "rcoder-docker-agent".to_string(),
            host_path: format!("{}/{}", host_workspace_dir, project_id),
            container_path: "/app/workspace".to_string(),
            work_dir: "/app/workspace".to_string(),
            env_vars,
            port_bindings: HashMap::new(), // 使用 host 网络
            network_mode: "host".to_string(),
            auto_remove: false,
            resource_limits: None,
            extra_mounts: Vec::new(),
            command: None,
        };

        Ok(config)
    }
}

// 为了简化，这里定义一些基本的请求/响应类型
// 在实际使用中，应该从 rcoder crate 中导入这些类型

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ChatRequest {
    pub prompt: String,
    pub session_id: Option<String>,
    pub project_id: String,
    pub model_provider: Option<ModelProviderConfig>,
    pub attachments: Vec<Attachment>,
    pub data_source_attachments: Vec<String>,
    pub request_id: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ChatResponse {
    pub session_id: String,
    pub request_id: String,
    pub status: String,
    pub content: Option<String>,
    pub error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct ModelProviderConfig {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub default_model: String,
    pub requires_openai_auth: bool,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub attachment_type: String,
    pub file_path: Option<String>,
    pub url: Option<String>,
    pub mime_type: String,
    pub size: Option<u64>,
}

use agent_client_protocol::ContainerStatus;
use dashmap::DashMap;
