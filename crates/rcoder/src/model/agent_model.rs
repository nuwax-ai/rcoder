use std::collections::HashMap;

use agent_client_protocol::SessionId;
use agent_client_protocol::{CancelNotification, PromptRequest};
use anyhow::Result;
use chrono::{DateTime, Utc};
use codex_core::WireApi;
use codex_core::{ModelProviderInfo, config::ConfigToml};
use shared_types::ModelProviderConfig;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};

use codex_core::config::{find_codex_home, load_config_as_toml};
use crate::proxy_agent::agent_stop_handle::{AgentLifecycleGuard};

pub static CUSTOM_MODEL_PROVIDER_NAME: &str = "custom";

pub static CUSTOM_MODEL_PROVIDER_API_KEY: &str = "API_KEY";

/// 使用Agent代理的工具类型,都是使用ACP协议包装过的agent代理
#[derive(Debug, Clone, Copy)]
pub enum AgentType {
    /// OpenAI Codex 代理
    Codex,
    /// Claude Code 代理
    Claude,
}

impl Default for AgentType {
    fn default() -> Self {
        Self::Claude
    }
}

impl AgentType {
    /// 根据模型提供商配置自动选择 Agent 类型
    /// - Anthropic 协议使用 Claude Code agent
    /// - OpenAI 或未知协议使用 Codex agent
    pub fn from_model_provider(model_provider: Option<&ModelProviderConfig>) -> Self {
        match model_provider {
            Some(config) => match config.get_api_protocol() {
                shared_types::ModelApiProtocol::Anthropic => AgentType::Claude,
                shared_types::ModelApiProtocol::OpenAI => AgentType::Codex,
            },
            None => AgentType::default(), // 默认使用 Claude
        }
    }

    /// 获取 codex 环境变量的模型提供商配置
    pub fn codex_from_env() -> Result<ConfigToml> {
        // 加载配置
        // 首先获取codex home目录 (~/.codex)
        let codex_home = find_codex_home().map_err(|e| {
            error!("Failed to find codex home directory: {}", e);
            anyhow::anyhow!("Failed to find codex home directory: {}", e)
        })?;

        info!("Codex home directory: {:?}", codex_home);

        // 从 ~/.codex/config.toml 加载配置
        let config_toml_value = load_config_as_toml(&codex_home).map_err(|e| {
            error!("Failed to load config.toml from {:?}: {}", codex_home, e);
            anyhow::anyhow!("Failed to load config.toml from {:?}: {}", codex_home, e)
        })?;

        // 将TOML值转换为ConfigToml结构体
        let cfg: ConfigToml = config_toml_value.try_into().map_err(|e| {
            error!("Failed to deserialize config.toml: {}", e);
            anyhow::anyhow!("Failed to deserialize config.toml: {}", e)
        })?;

        Ok(cfg)
    }

    /// 获取 codex 模型提供商配置
    #[allow(dead_code)]
    pub fn codex_model_provider(
        model_provider: Option<ModelProviderConfig>,
    ) -> Result<(ConfigToml, HashMap<String, String>)> {
        let result = match model_provider {
            Some(model_provider) => {
                let mut merged_envs: HashMap<String, String> = HashMap::new();
                let api_key_name = CUSTOM_MODEL_PROVIDER_API_KEY.to_string();
                let api_key_value = model_provider.api_key.clone();
                merged_envs.insert(api_key_name.clone(), api_key_value);
                // 加载配置
                // 首先获取codex home目录 (~/.codex)。失败则直接使用默认配置
                let mut cfg: ConfigToml = match find_codex_home() {
                    Ok(codex_home) => {
                        info!("Codex home directory: {:?}", codex_home);
                        // 从 ~/.codex/config.toml 加载配置；失败时回退到默认配置
                        match load_config_as_toml(&codex_home) {
                            Ok(value) => match value.try_into() {
                                Ok(cfg) => cfg,
                                Err(e) => {
                                    warn!(
                                        "Failed to deserialize config.toml, using defaults: {}",
                                        e
                                    );
                                    ConfigToml::default()
                                }
                            },
                            Err(e) => {
                                warn!(
                                    "Failed to load config.toml from {:?}, using defaults: {}",
                                    codex_home, e
                                );
                                ConfigToml::default()
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to find codex home directory, using defaults: {}", e);
                        ConfigToml::default()
                    }
                };
                //默认添加 custom 模型提供商
                cfg.model_provider = Some(CUSTOM_MODEL_PROVIDER_NAME.to_string());

                info!("Loaded codex config: {:?}", cfg);
                // 基于入参覆盖/扩展 model_providers（不修改进程环境变量）

                // 构造 ModelProviderInfo
                let provider_info = ModelProviderInfo {
                    name: model_provider.name.clone(),
                    base_url: if model_provider.base_url.is_empty() {
                        None
                    } else {
                        Some(model_provider.base_url.clone())
                    },
                    env_key: Some(api_key_name.clone()),
                    env_key_instructions: None,
                    wire_api: WireApi::default(),
                    query_params: None,
                    http_headers: None,
                    env_http_headers: None,
                    request_max_retries: None,
                    stream_max_retries: None,
                    stream_idle_timeout_ms: None,
                    requires_openai_auth: model_provider.requires_openai_auth,
                };

                // 覆盖/写入到 cfg.model_providers（若同名则替换）
                cfg.model_providers
                    .insert(CUSTOM_MODEL_PROVIDER_NAME.to_string(), provider_info);
                (cfg, merged_envs)
            }
            None => {
                let result = AgentType::codex_from_env()?;
                (result, HashMap::new())
            }
        };

        Ok(result)
    }

    /// 获取 claude code 环境变量的模型提供商配置
    pub fn claude_from_env() -> Result<HashMap<String, String>> {
        // 合并命令自带 env 与当前进程中的必需 ANTHROPIC_* 环境变量
        let mut merged_envs: std::collections::HashMap<String, String> = HashMap::new();
        for key in [
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_MODEL",
            "ANTHROPIC_SMALL_FAST_MODEL",
        ] {
            if let Ok(val) = std::env::var(key) {
                merged_envs.insert(key.to_string(), val);
            }
        }
        //固定开启 yolo 模式
        merged_envs.insert(
            "CLAUDE_CODE_ARGS".to_string(),
            "--dangerously-skip-permissions".to_string(),
        );
        Ok(merged_envs)
    }

    /// 获取 claude code 模型提供商配置
    #[allow(dead_code)]
    pub fn claude_model_provider(
        model_provider: Option<ModelProviderConfig>,
    ) -> Result<HashMap<String, String>> {
        let result = match model_provider {
            Some(model_provider) => {
                // 先从入参填充，再用环境变量覆盖（环境变量优先级更高）
                let mut merged_envs: HashMap<String, String> = HashMap::new();

                // 从 model_provider 映射到 Anthropic 所需键
                if !model_provider.base_url.is_empty() {
                    merged_envs.insert(
                        "ANTHROPIC_BASE_URL".to_string(),
                        model_provider.base_url.clone(),
                    );
                }
                if !model_provider.api_key.is_empty() {
                    merged_envs.insert(
                        "ANTHROPIC_AUTH_TOKEN".to_string(),
                        model_provider.api_key.clone(),
                    );
                }
                if !model_provider.default_model.is_empty() {
                    merged_envs.insert(
                        "ANTHROPIC_MODEL".to_string(),
                        model_provider.default_model.clone(),
                    );
                    merged_envs.insert(
                        "ANTHROPIC_SMALL_FAST_MODEL".to_string(),
                        model_provider.default_model.clone(),
                    );
                }
                //固定开启 yolo 模式
                merged_envs.insert(
                    "CLAUDE_CODE_ARGS".to_string(),
                    "--dangerously-skip-permissions".to_string(),
                );
                merged_envs
            }
            None => AgentType::claude_from_env()?,
        };

        Ok(result)
    }
}

/// 取消通知请求
pub struct CancelNotificationRequest {
    pub cancel_notification: CancelNotification,
    pub tx: oneshot::Sender<CancelNotificationResponse>,
}

/// 取消通知响应
pub struct CancelNotificationResponse {
    pub success: bool,
    pub message: Option<String>,
}
/// Agent 服务状态
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub enum AgentStatus {
    /// 活跃状态 - 正在处理请求
    Active,
    /// 空闲状态 - 等待新请求
    Idle,
    /// 正在终止
    Terminating,
}

/// 项目id与 Agent 服务池，一个项目对应一个 Agent 服务
#[derive(Clone)]
pub struct ProjectAndAgentInfo {
    /// 项目ID
    pub project_id: String,
    /// 会话ID,agent 服务启动时会创建一个会话ID
    pub session_id: SessionId,
    /// 用于发送 Prompt 的通道
    pub prompt_tx: mpsc::UnboundedSender<PromptRequest>,
    /// 用于发送取消通知的通道
    pub cancel_tx: mpsc::UnboundedSender<CancelNotificationRequest>,
    /// 模型提供商配置
    pub model_provider: Option<ModelProviderConfig>,
    /// 当前活跃的请求ID，用于标识用户请求
    pub request_id: Option<String>,
    /// Agent 服务状态
    pub status: AgentStatus,
    /// 最后活动时间
    pub last_activity: DateTime<Utc>,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// Agent生命周期守卫，绑定生命周期，drop 时自动清理
    pub lifecycle_guard: AgentLifecycleGuard,
    /// Agent是否正在停止
    pub is_stopping: bool,
}

impl Drop for ProjectAndAgentInfo {
    fn drop(&mut self) {
        // 生命周期守卫会自动在drop时清理agent资源
        info!(
            "ProjectAndAgentInfo被drop，生命周期守卫将自动清理agent服务，项目ID: {}",
            self.project_id
        );
    }
}
