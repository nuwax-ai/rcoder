use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use dashmap::DashMap;
use docker_manager::{
    DockerContainerConfig, DockerContainerInfo, DockerManager, DockerManagerConfig, DockerUtils,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, warn};

use crate::{
    AgentStatus, AgentType, ChatPrompt, ChatPromptResponse, ProjectAndAgentInfo,
    proxy_agent::agent_stop_handle::AgentLifecycleGuard,
};

/// Docker Agent 管理器
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
    /// 创建时间
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl DockerAgentManager {
    /// 创建新的 Docker Agent 管理器
    pub async fn new() -> Result<Self> {
        let config = DockerUtils::config_from_env();

        // 🔍 调试日志：打印初始化配置
        info!("🔍 DockerAgentManager 初始化配置:");
        info!("  - default_platform: {}", config.default_platform);
        info!("  - default_image: {}", config.default_image);

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

    /// 创建 Docker Agent 服务
    pub async fn create_docker_agent(
        &self,
        chat_prompt: &ChatPrompt,
        agent_type: AgentType,
    ) -> Result<DockerAgentInfo> {
        info!(
            "开始创建 Docker Agent，项目ID: {}, Agent类型: {:?}",
            chat_prompt.project_id, agent_type
        );

        // 检查是否已存在该项目的 Docker Agent
        if let Some(existing) = self.agents.get(&chat_prompt.project_id) {
            warn!(
                "项目 {} 已存在 Docker Agent，将先停止",
                chat_prompt.project_id
            );
            self.stop_docker_agent(&chat_prompt.project_id).await?;
        }

        // 创建 Docker 容器配置
        let mut config = self.create_docker_config(chat_prompt, agent_type).await?;

        // 确保项目目录存在
        if !std::path::Path::new(&config.host_path).exists() {
            tokio::fs::create_dir_all(&config.host_path).await?;
            info!("创建项目目录: {}", config.host_path);
        }

        // 创建并启动容器
        let container_info = self.docker_manager.create_container(config).await?;

        // 创建 Docker Agent 信息
        let docker_agent_info = DockerAgentInfo {
            project_id: chat_prompt.project_id.clone(),
            container_info: container_info.clone(),
            agent_type,
            created_at: Utc::now(),
        };

        // 保存到映射
        self.agents
            .insert(chat_prompt.project_id.clone(), docker_agent_info.clone());

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
    pub async fn check_agent_status(
        &self,
        project_id: &str,
    ) -> Result<Option<docker_manager::ContainerStatus>> {
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

    /// 创建 Docker 容器配置
    async fn create_docker_config(
        &self,
        chat_prompt: &ChatPrompt,
        agent_type: AgentType,
    ) -> Result<DockerContainerConfig> {
        let mut env_vars = HashMap::new();

        // 根据 Agent 类型设置环境变量
        match agent_type {
            AgentType::Claude => {
                // Claude Code 环境变量
                if let Some(model_provider) = &chat_prompt.model_provider {
                    if !model_provider.base_url.is_empty() {
                        env_vars.insert(
                            "ANTHROPIC_BASE_URL".to_string(),
                            model_provider.base_url.clone(),
                        );
                    }
                    if !model_provider.api_key.is_empty() {
                        env_vars.insert(
                            "ANTHROPIC_AUTH_TOKEN".to_string(),
                            model_provider.api_key.clone(),
                        );
                    }
                    if !model_provider.default_model.is_empty() {
                        env_vars.insert(
                            "ANTHROPIC_MODEL".to_string(),
                            model_provider.default_model.clone(),
                        );
                        env_vars.insert(
                            "ANTHROPIC_SMALL_FAST_MODEL".to_string(),
                            model_provider.default_model.clone(),
                        );
                    }
                }

                // 固定开启 yolo 模式
                env_vars.insert(
                    "CLAUDE_CODE_ARGS".to_string(),
                    "--dangerously-skip-permissions".to_string(),
                );

                // 添加项目 ID 环境变量
                env_vars.insert("PROJECT_ID".to_string(), chat_prompt.project_id.clone());

                // 添加会话 ID 环境变量
                if let Some(session_id) = &chat_prompt.session_id {
                    env_vars.insert("SESSION_ID".to_string(), session_id.clone());
                }
            }
            AgentType::Codex => {
                // Codex 环境变量
                if let Some(model_provider) = &chat_prompt.model_provider {
                    let api_key_value = model_provider.api_key.clone();
                    env_vars.insert("API_KEY".to_string(), api_key_value.clone());
                    env_vars.insert("OPENAI_API_KEY".to_string(), api_key_value.clone());
                    env_vars.insert("CODEX_API_KEY".to_string(), api_key_value.clone());

                    if !model_provider.base_url.is_empty() {
                        env_vars.insert(
                            "OPENAI_API_BASE".to_string(),
                            model_provider.base_url.clone(),
                        );
                    }
                    if !model_provider.default_model.is_empty() {
                        env_vars.insert(
                            "OPENAI_MODEL".to_string(),
                            model_provider.default_model.clone(),
                        );
                    }
                }

                // 添加项目 ID 环境变量
                env_vars.insert("PROJECT_ID".to_string(), chat_prompt.project_id.clone());
            }
        }

        // 添加通用环境变量
        env_vars.insert("RUST_LOG".to_string(), "info".to_string());
        env_vars.insert("TZ".to_string(), "Asia/Shanghai".to_string());
        env_vars.insert("DOCKER_AGENT".to_string(), "true".to_string());

        // 创建容器配置
        let config = DockerContainerConfig {
            project_id: chat_prompt.project_id.clone(),
            image: docker_manager::default_docker_image(),
            name_prefix: "rcoder-docker-agent".to_string(),
            host_path: chat_prompt.project_path.to_string_lossy().to_string(),
            container_path: "/app/workspace".to_string(),
            work_dir: "/app/workspace".to_string(),
            env_vars,
            port_bindings: HashMap::new(),
            network_mode: "host".to_string(),
            auto_remove: false,
            resource_limits: None,
            extra_mounts: Vec::new(),
            command: None,
        };

        Ok(config)
    }
}

/// Docker Agent 生命周期守卫
pub struct DockerAgentLifecycleGuard {
    project_id: String,
    docker_manager: Arc<DockerAgentManager>,
}

impl DockerAgentLifecycleGuard {
    pub fn new(project_id: String, docker_manager: Arc<DockerAgentManager>) -> Self {
        Self {
            project_id,
            docker_manager,
        }
    }

    /// 异步停止 Docker Agent
    pub async fn stop_async(&self) -> Result<()> {
        self.docker_manager
            .stop_docker_agent(&self.project_id)
            .await
    }
}

impl Drop for DockerAgentLifecycleGuard {
    fn drop(&mut self) {
        let project_id = self.project_id.clone();
        let docker_manager = self.docker_manager.clone();

        // 在异步运行时中停止 Docker Agent
        tokio::spawn(async move {
            if let Err(e) = docker_manager.stop_docker_agent(&project_id).await {
                error!("DockerAgentLifecycleGuard drop 时停止 Agent 失败: {}", e);
            }
        });
    }
}
