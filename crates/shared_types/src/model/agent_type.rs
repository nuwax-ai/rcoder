//! Agent 类型定义 - rcoder 和 agent_runner 共用

use super::model_provider::ModelProviderConfig;
use anyhow::Result;
#[cfg(feature = "codex")]
use codex_core::{
    ModelProviderInfo, WireApi,
    config::{ConfigToml, find_codex_home, load_config_as_toml},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{error, info, warn};

#[cfg(feature = "codex")]
pub static CUSTOM_MODEL_PROVIDER_NAME: &str = "custom";

/// 使用Agent代理的工具类型,都是使用ACP协议包装过的agent代理
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentType {
    /// OpenAI Codex 代理
    #[cfg(feature = "codex")]
    Codex,
    /// Claude Code 代理
    Claude,
}

impl Default for AgentType {
    fn default() -> Self {
        Self::Claude
    }
}

impl std::fmt::Display for AgentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(feature = "codex")]
            AgentType::Codex => write!(f, "codex"),
            AgentType::Claude => write!(f, "claude"),
        }
    }
}

impl std::str::FromStr for AgentType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            #[cfg(feature = "codex")]
            "codex" => Ok(AgentType::Codex),
            "claude" => Ok(AgentType::Claude),
            _ => {
                #[cfg(feature = "codex")]
                let valid_types = "codex, claude";
                #[cfg(not(feature = "codex"))]
                let valid_types = "claude";
                
                Err(format!(
                    "Invalid agent type: {}. Must be one of: {}",
                    s, valid_types
                ))
            }
        }
    }
}

impl AgentType {
    /// 获取 codex 环境变量的模型提供商配置
    #[cfg(feature = "codex")]
    pub async fn codex_from_env() -> Result<ConfigToml> {
        // 加载配置
        // 首先获取codex home目录 (~/.codex)
        let codex_home = find_codex_home().map_err(|e| {
            error!("Failed to find codex home directory: {}", e);
            anyhow::anyhow!("Failed to find codex home directory: {}", e)
        })?;

        info!("Codex home directory: {:?}", codex_home);

        // 从 ~/.codex/config.toml 加载配置
        let config_toml_value = load_config_as_toml(&codex_home).await.map_err(|e| {
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

    /// 从模型提供商配置推断 Agent 类型
    pub fn from_model_provider(model_provider: Option<&ModelProviderConfig>) -> Self {
        match model_provider {
            Some(provider) => match provider.name.as_str() {
                "anthropic" => AgentType::Claude,
                #[cfg(feature = "codex")]
                "openai" => AgentType::Codex,
                _ => AgentType::Claude, // 默认使用 Claude
            },
            None => AgentType::Claude, // 默认使用 Claude
        }
    }

    /// 获取 codex 模型提供商配置
    #[cfg(feature = "codex")]
    #[allow(dead_code)]
    pub async fn codex_model_provider(
        model_provider: Option<ModelProviderConfig>,
    ) -> Result<(ConfigToml, HashMap<String, String>)> {
        let result = match model_provider {
            Some(model_provider) => {
                let mut merged_envs: HashMap<String, String> = HashMap::new();
                let api_key_value = model_provider.api_key.clone();
                // 同时设置两个环境变量，确保 codex-acp-agent 能识别
                merged_envs.insert("API_KEY".to_string(), api_key_value.clone());
                merged_envs.insert("OPENAI_API_KEY".to_string(), api_key_value.clone());
                merged_envs.insert("CODEX_API_KEY".to_string(), api_key_value.clone());
                // 加载配置
                // 首先获取codex home目录 (~/.codex)。失败则直接使用默认配置
                let mut cfg: ConfigToml = match find_codex_home() {
                    Ok(codex_home) => {
                        info!("Codex home directory: {:?}", codex_home);
                        // 从 ~/.codex/config.toml 加载配置;失败时回退到默认配置
                        match load_config_as_toml(&codex_home).await {
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
                    env_key: Some("OPENAI_API_KEY".to_string()),
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
                let result = AgentType::codex_from_env().await?;
                (result, HashMap::new())
            }
        };

        Ok(result)
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
