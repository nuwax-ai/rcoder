use std::collections::HashMap;

use agent_config::AgentServersConfig;
use anyhow::Result;
use shared_types::ModelProviderConfig;
use tracing::{debug, warn};

use super::env::render_model_template;
use super::types::{
    ENV_AGENT_SDK_SKIP_VERSION_CHECK, ENV_ANTHROPIC_API_KEY, ENV_ANTHROPIC_BASE_URL,
    ENV_ANTHROPIC_MODEL, ENV_DISABLE_NONESSENTIAL, ENV_OPENAI_API_KEY, ENV_OPENAI_BASE_URL,
    ENV_OPENCODE_MODEL, ENV_RUST_LOG, SacpAgentLaunchConfig,
};
use crate::launcher::model_env::{DirectModelRuntimeEnvResolver, ModelRuntimeEnvResolver};

/// 从配置文件加载 Agent 配置
///
/// 优先加载嵌入的JSON配置文件，如果加载失败则使用默认配置
/// 同时检查并自动安装 agent（如果需要）
pub async fn load_sacp_agent_config(
    model_provider: Option<&ModelProviderConfig>,
    service_type: &shared_types::ServiceType,
) -> Result<SacpAgentLaunchConfig> {
    let resolver = DirectModelRuntimeEnvResolver;
    load_sacp_agent_config_with_resolver(model_provider, service_type, &resolver, None).await
}

pub async fn load_sacp_agent_config_with_resolver(
    model_provider: Option<&ModelProviderConfig>,
    service_type: &shared_types::ServiceType,
    model_env_resolver: &dyn ModelRuntimeEnvResolver,
    service_uuid: Option<&str>,
) -> Result<SacpAgentLaunchConfig> {
    // 复用旧版配置加载逻辑
    let config = AgentServersConfig::load_or_default_for_service(service_type).await;

    if let Some(agent_config) = config.get_agent("claude-code-acp-ts") {
        debug!(
            "📋 [SACP] using default Agent config: {}",
            agent_config.agent_id
        );

        // 检查并安装 agent - 临时禁用以测试本地 claude-code-acp-ts
        // if agent_config.installation.package_name.is_some() {
        //     let installation_manager = AgentInstallationManager::new();
        //     match installation_manager
        //         .ensure_installed(&agent_config.installation, &agent_config.command)
        //         .await
        //     {
        //         Ok(result) => {
        //             if result.already_installed {
        //                 debug!("[SACP] Agent already installed: {}", agent_config.command);
        //             } else {
        //                 info!("[SACP] Agent message succeeded: {}", result.message);
        //             }
        //         }
        //         Err(e) => {
        //             warn!(
        //                 "[SACP] Agent message Installation failed: {}, message started",
        //                 e
        //             );
        //         }
        //     }
        // }

        // 解析环境变量
        let mut resolved_env = agent_config.env.clone();

        if let Some(provider) = model_provider {
            let resolved = model_env_resolver.resolve(provider, service_uuid)?;
            for (_key, value) in resolved_env.iter_mut() {
                render_model_template(value, &resolved);
            }
        }

        // 禁用 Claude Code 非必要网络请求
        resolved_env.insert(ENV_DISABLE_NONESSENTIAL.to_string(), "1".to_string());
        // 跳过 Agent SDK 版本检查
        resolved_env.insert(
            ENV_AGENT_SDK_SKIP_VERSION_CHECK.to_string(),
            "1".to_string(),
        );

        // debug: 打印最终环境变量（API Key 已脱敏）
        let mask_key = |v: &String| -> String {
            if v.len() > 8 {
                format!("{}***{}", &v[..4], &v[v.len() - 4..])
            } else {
                "***".to_string()
            }
        };
        debug!(
            "[SACP] Final env config: command={}, ANTHROPIC_API_KEY={}, ANTHROPIC_BASE_URL={}, ANTHROPIC_MODEL={}, \
             OPENAI_API_KEY={}, OPENAI_BASE_URL={}, OPENCODE_MODEL={}, \
             RUST_LOG={}, CLAUDE_CODE_MAX_TOKENS={}, CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC={}",
            agent_config.command,
            resolved_env
                .get("ANTHROPIC_API_KEY")
                .map(&mask_key)
                .unwrap_or_default(),
            resolved_env
                .get("ANTHROPIC_BASE_URL")
                .unwrap_or(&"<unset>".to_string()),
            resolved_env
                .get("ANTHROPIC_MODEL")
                .unwrap_or(&"<unset>".to_string()),
            resolved_env
                .get("OPENAI_API_KEY")
                .map(mask_key)
                .unwrap_or_default(),
            resolved_env
                .get("OPENAI_BASE_URL")
                .unwrap_or(&"<unset>".to_string()),
            resolved_env
                .get("OPENCODE_MODEL")
                .unwrap_or(&"<unset>".to_string()),
            resolved_env
                .get("RUST_LOG")
                .unwrap_or(&"<unset>".to_string()),
            resolved_env
                .get("CLAUDE_CODE_MAX_TOKENS")
                .unwrap_or(&"<unset>".to_string()),
            resolved_env
                .get("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC")
                .unwrap_or(&"<unset>".to_string()),
        );

        Ok(SacpAgentLaunchConfig {
            command: agent_config.command.clone(),
            args: agent_config.args.clone(),
            env: resolved_env,
            context_servers: config.context_servers.clone(),
        })
    } else {
        warn!("[SACP] config not found for claude-code-acp-ts, using default config");
        get_default_sacp_agent_config_with_resolver(
            model_provider,
            service_type,
            model_env_resolver,
            service_uuid,
        )
    }
}

/// 获取默认的 Agent 配置（后备方案）
pub fn get_default_sacp_agent_config(
    model_provider: Option<&ModelProviderConfig>,
    service_type: &shared_types::ServiceType,
) -> Result<SacpAgentLaunchConfig> {
    let resolver = DirectModelRuntimeEnvResolver;
    get_default_sacp_agent_config_with_resolver(model_provider, service_type, &resolver, None)
}

pub fn get_default_sacp_agent_config_with_resolver(
    model_provider: Option<&ModelProviderConfig>,
    _service_type: &shared_types::ServiceType,
    model_env_resolver: &dyn ModelRuntimeEnvResolver,
    service_uuid: Option<&str>,
) -> Result<SacpAgentLaunchConfig> {
    let mut env = HashMap::new();

    if let Some(provider) = model_provider {
        let resolved = model_env_resolver.resolve(provider, service_uuid)?;

        // Anthropic 环境变量
        if !provider.api_key.is_empty() {
            env.insert(ENV_ANTHROPIC_API_KEY.to_string(), resolved.api_key.clone());
        }
        if !provider.base_url.is_empty() {
            env.insert(
                ENV_ANTHROPIC_BASE_URL.to_string(),
                resolved.base_url.clone(),
            );
        }
        if !provider.default_model.is_empty() {
            env.insert(
                ENV_ANTHROPIC_MODEL.to_string(),
                resolved.default_model.clone(),
            );
        }

        // OpenAI 环境变量 (支持 OpenAI 兼容的 Agent)
        if !provider.api_key.is_empty() {
            env.insert(ENV_OPENAI_API_KEY.to_string(), resolved.api_key);
        }
        if !provider.base_url.is_empty() {
            env.insert(ENV_OPENAI_BASE_URL.to_string(), resolved.base_url);
        }
        if !provider.default_model.is_empty() {
            // nuwaxcode 使用 OPENCODE_MODEL，model_name 中已包含 openai-compatible/ 前缀
            env.insert(
                ENV_OPENCODE_MODEL.to_string(),
                resolved.default_model.clone(),
            );
        }
    }

    env.insert(ENV_RUST_LOG.to_string(), "info".to_string());
    env.insert(ENV_DISABLE_NONESSENTIAL.to_string(), "1".to_string());
    env.insert(
        ENV_AGENT_SDK_SKIP_VERSION_CHECK.to_string(),
        "1".to_string(),
    );

    // Resolve the claude-code-acp-ts command path.
    // Priority: CLAUDE_CODE_ACP_PATH env var > `which` crate lookup > bare command name.
    // Tauri apps may not inherit the user's shell PATH, so we try `which` crate to get
    // an absolute path at build/launch time.
    let command = if let Ok(path) = std::env::var("CLAUDE_CODE_ACP_PATH") {
        path
    } else {
        match which::which("claude-code-acp-ts") {
            Ok(resolved_path) => {
                tracing::info!(
                    "Resolved claude-code-acp-ts path via `which` crate: {}",
                    resolved_path.display()
                );
                resolved_path.to_string_lossy().to_string()
            }
            Err(_) => "claude-code-acp-ts".to_string(),
        }
    };

    Ok(SacpAgentLaunchConfig {
        command,
        args: Vec::new(),
        env,
        context_servers: HashMap::new(),
    })
}
