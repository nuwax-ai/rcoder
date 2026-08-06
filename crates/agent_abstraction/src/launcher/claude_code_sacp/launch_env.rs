//! 启动环境解析
//!
//! 从 `SacpClaudeCodeLauncher::launch` 抽出的环境解析部分：
//! - 模型环境解析与默认 Agent 配置加载
//! - agent_server 覆盖合并（命令/参数/环境变量/模板渲染/model env bindings）
//! - `{PREFIX_WORKSPACE_DIR}` 占位符解析
//! - 平台 cfg 分支（Windows node CLI 解析与无窗口规范化）
//! - 子进程最终环境变量合并（工作目录/权限/PATH/SERVICE_UUID/敏感回退）

use std::collections::{HashMap, HashSet};
use std::path::Path;

#[cfg(windows)]
use super::super::windows_launch::{
    normalize_windows_command_for_no_window, resolve_windows_node_cli_command,
};
use super::config::load_sacp_agent_config_with_resolver;
use super::env::{
    apply_model_env_bindings, apply_sensitive_model_env_fallback, ensure_subprocess_path_env,
    render_model_template, render_prefix_workspace_dir,
};
use super::types::{
    ENV_AGENT_PROJECT_ID, ENV_AGENT_WORKING_DIR, ENV_ANTHROPIC_API_KEY, ENV_ANTHROPIC_BASE_URL,
    ENV_CODEX_API_KEY, ENV_OPENAI_API_KEY, ENV_OPENAI_BASE_URL, ENV_OPENCODE_PERMISSION,
    SacpAgentLaunchConfig,
};
use crate::launcher::model_env::{ModelRuntimeEnvResolver, ResolvedModelEnv};
use crate::traits::AgentStartConfig;
use anyhow::Result;
use shared_types::ModelProviderConfig;
use tracing::{debug, info, warn};

/// 命令与基础环境解析结果（覆盖合并 + 占位符渲染后的中间态）
pub(super) struct ResolvedAgentCommand {
    /// 默认 Agent 配置（后续 MCP servers 解析仍需使用）
    pub(super) default_agent_config: SacpAgentLaunchConfig,
    pub(super) resolved_model_env: Option<ResolvedModelEnv>,
    pub(super) command_path: String,
    pub(super) command_args: Vec<String>,
    pub(super) base_env: HashMap<String, String>,
    pub(super) explicitly_bound_model_env_keys: HashSet<String>,
}

/// 子进程最终启动环境
pub(super) struct FinalizedLaunchEnv {
    pub(super) command_path: String,
    pub(super) command_args: Vec<String>,
    pub(super) merged_envs: HashMap<String, String>,
    pub(super) full_command_line: String,
}

/// 解析 Agent 命令与基础环境：模型环境解析、agent_server 覆盖合并、
/// `{PREFIX_WORKSPACE_DIR}` 占位符渲染
pub(super) async fn resolve_agent_command(
    model_env_resolver: &dyn ModelRuntimeEnvResolver,
    model_provider: Option<&ModelProviderConfig>,
    start_config: &AgentStartConfig,
    service_uuid: Option<&str>,
) -> Result<ResolvedAgentCommand> {
    // 从配置加载默认 Agent 参数
    let resolved_model_env = model_provider
        .map(|provider| model_env_resolver.resolve(provider, service_uuid))
        .transpose()?;
    let default_agent_config = load_sacp_agent_config_with_resolver(
        model_provider,
        &start_config.service_type,
        model_env_resolver,
        service_uuid,
    )
    .await?;

    // 🎯 关键：检查是否有自定义 agent_server 配置覆盖
    let (command_path, command_args, base_env, explicitly_bound_model_env_keys) = if let Some(
        ref agent_server_override,
    ) =
        start_config.agent_server_override
    {
        // 使用自定义 command（如果提供），否则用默认
        let cmd = agent_server_override
            .command
            .clone()
            .unwrap_or_else(|| default_agent_config.command.clone());

        // 使用自定义 args（如果提供），否则用默认
        let args = agent_server_override
            .args
            .clone()
            .unwrap_or_else(|| default_agent_config.args.clone());

        // 合并环境变量：默认配置 + 自定义配置（自定义覆盖默认）
        let mut env = default_agent_config.env.clone();
        if let Some(custom_env) = &agent_server_override.env {
            // 使用 extend 替代循环，更高效
            env.extend(custom_env.iter().map(|(k, v)| (k.clone(), v.clone())));
        }

        // 🔧 关键修复：替换自定义环境变量中的模板变量
        // 用户可能传入 {MODEL_PROVIDER_API_KEY} 等模板，需要替换为实际值
        if let Some(ref resolved) = resolved_model_env {
            for value in env.values_mut() {
                render_model_template(value, resolved);
            }
            let bound_model_env_keys = apply_model_env_bindings(
                &mut env,
                &agent_server_override.model_env_bindings,
                resolved,
            );
            debug!(
                "[SACP] Replaced custom env var template, model={}",
                resolved.default_model
            );
            info!(
                "[SACP] Applied {} model env bindings",
                bound_model_env_keys.len()
            );
            info!(
                "[SACP] Using custom Agent: agent_id={}, command={} {:?}",
                agent_server_override.get_agent_id(),
                cmd,
                args
            );
            (cmd, args, env, bound_model_env_keys)
        } else {
            // model_provider 为 None，模板变量无法解析
            // 检查 env 中是否仍包含未解析的模板占位符
            let unresolved_keys: Vec<_> = env
                .iter()
                .filter(|(_, v)| {
                    v.contains("{MODEL_PROVIDER_API_KEY}")
                        || v.contains("{MODEL_PROVIDER_BASE_URL}")
                        || v.contains("{MODEL_PROVIDER_DEFAULT_MODEL}")
                        || v.contains("{MODEL_PROVIDER_NAME}")
                })
                .map(|(k, _)| k.clone())
                .collect();
            if !unresolved_keys.is_empty() {
                warn!(
                    "[SACP] model_provider is None, {} env keys contain unresolved template placeholders: {:?}. Agent may fail due to invalid config.",
                    unresolved_keys.len(),
                    unresolved_keys
                );
            } else if !agent_server_override.model_env_bindings.is_empty() {
                warn!(
                    "[SACP] model_env_bindings configured but model_provider is missing; bindings were not applied"
                );
            }
            info!(
                "[SACP] Using custom Agent: agent_id={}, command={} {:?}",
                agent_server_override.get_agent_id(),
                cmd,
                args
            );
            (cmd, args, env, HashSet::new())
        }
    } else {
        // 使用默认配置
        info!(
            "[SACP] Using default Agent: {} {:?}",
            default_agent_config.command, default_agent_config.args
        );
        (
            default_agent_config.command.clone(),
            default_agent_config.args.clone(),
            default_agent_config.env.clone(),
            HashSet::new(),
        )
    };

    // 🔧 解析 {PREFIX_WORKSPACE_DIR} 占位符
    // 根据不同场景解析为不同的路径：
    // - 环境变量 LOG_DIR/OPENCODE_LOG_DIR + devcomputer → /home/user/
    // - 环境变量 LOG_DIR/OPENCODE_LOG_DIR + computer → /app/container-logs
    // - command / args → /home/user/
    let is_devcomputer = start_config.is_devcomputer;

    // 解析 command 中的 {PREFIX_WORKSPACE_DIR}
    let mut resolved_command = command_path.clone();
    render_prefix_workspace_dir(&mut resolved_command, None, is_devcomputer);

    // 解析 args 中的 {PREFIX_WORKSPACE_DIR}
    let resolved_args: Vec<String> = command_args
        .iter()
        .map(|arg| {
            let mut arg = arg.clone();
            render_prefix_workspace_dir(&mut arg, None, is_devcomputer);
            arg
        })
        .collect();

    // 解析环境变量中的 {PREFIX_WORKSPACE_DIR}
    let resolved_env: HashMap<String, String> = base_env
        .iter()
        .map(|(k, v)| {
            let mut v = v.clone();
            render_prefix_workspace_dir(&mut v, Some(k), is_devcomputer);
            (k.clone(), v)
        })
        .collect();

    // 使用解析后的值
    Ok(ResolvedAgentCommand {
        default_agent_config,
        resolved_model_env,
        command_path: resolved_command,
        command_args: resolved_args,
        base_env: resolved_env,
        explicitly_bound_model_env_keys,
    })
}

/// 确定子进程最终启动环境：平台分支、工作目录/权限/PATH 注入、
/// `{SERVICE_UUID}` 替换、敏感变量回退与环境变量打印
pub(super) fn finalize_subprocess_env(
    resolved: ResolvedAgentCommand,
    project_id: &str,
    project_path: &Path,
    start_config: &AgentStartConfig,
    service_uuid: Option<&str>,
) -> FinalizedLaunchEnv {
    let ResolvedAgentCommand {
        default_agent_config: _,
        resolved_model_env,
        command_path,
        command_args,
        base_env,
        explicitly_bound_model_env_keys,
    } = resolved;

    // Windows 平台需要解析 node CLI 命令路径
    #[cfg(windows)]
    let (command_path, command_args) = {
        if let Some((resolved_program, resolved_args)) =
            resolve_windows_node_cli_command(&command_path, &command_args)
        {
            let entry = resolved_args.first().cloned().unwrap_or_default();
            info!(
                "[SACP] Windows direct node startup: {} -> {} {}",
                command_path, resolved_program, entry
            );
            (resolved_program, resolved_args)
        } else {
            (command_path, command_args)
        }
    };

    // 准备环境变量（在 base_env 基础上添加项目相关变量）
    let mut merged_envs = base_env;
    merged_envs.insert(
        ENV_AGENT_WORKING_DIR.to_string(),
        project_path.to_string_lossy().to_string(),
    );
    merged_envs.insert(ENV_AGENT_PROJECT_ID.to_string(), project_id.to_string());

    // 设置子进程的权限模式（ask 和 yolo 模式均生效）
    // nuwaxcode: 通过 OPENCODE_PERMISSION 环境变量控制工具权限，由 permission_manager 根据 tool_approval_rules 做最终决策
    // claude-code-acp-ts: 通过 ACP SetSessionModeRequest 设置 "default" 模式（在 connection.rs 中处理）
    let cmd_lower = command_path.to_lowercase();
    if cmd_lower.contains("nuwaxcode") || cmd_lower.contains("opencode") {
        // nuwaxcode 使用 OPENCODE_PERMISSION 环境变量控制具体工具权限
        // 请求中传入的值优先，未传时使用默认值（全部 ask）
        // permission_manager 会根据 tool_approval_rules 和 agent_mode 做最终决策
        if !merged_envs.contains_key(ENV_OPENCODE_PERMISSION) {
            let permission_config = serde_json::json!({
                "bash": "ask",
                "edit": "ask",
                "question": "deny"
            });
            merged_envs.insert(
                ENV_OPENCODE_PERMISSION.to_string(),
                permission_config.to_string(),
            );
            info!(
                "[SACP] Setting default OPENCODE_PERMISSION for nuwaxcode (agent_mode={:?}): {}",
                start_config.agent_mode, permission_config
            );
        } else if let Some(perm) = merged_envs.get(ENV_OPENCODE_PERMISSION) {
            info!(
                "[SACP] Using request-provided OPENCODE_PERMISSION: {}",
                perm
            );
        }
    }

    ensure_subprocess_path_env(&mut merged_envs);

    // 🔍 调试：打印替换前的关键环境变量
    info!(
        "[SACP] Before UUID replacement: OPENAI_BASE_URL={}, ANTHROPIC_BASE_URL={}, service_uuid={:?}",
        merged_envs
            .get(ENV_OPENAI_BASE_URL)
            .map(|s| s.as_str())
            .unwrap_or("<unset>"),
        merged_envs
            .get(ENV_ANTHROPIC_BASE_URL)
            .map(|s| s.as_str())
            .unwrap_or("<unset>"),
        service_uuid
    );

    // 替换 UUID 占位符
    if let Some(uuid) = service_uuid {
        info!("[SACP] Replacing {{SERVICE_UUID}} with: {}", uuid);
        for value in merged_envs.values_mut() {
            *value = value.replace("{SERVICE_UUID}", uuid);
        }
    } else {
        warn!("[SACP] service_uuid is None, UUID placeholder will NOT be replaced!");
    }

    // 🔍 调试：打印替换后的关键环境变量
    info!(
        "[SACP] After UUID replacement: OPENAI_BASE_URL={}, ANTHROPIC_BASE_URL={}",
        merged_envs
            .get(ENV_OPENAI_BASE_URL)
            .map(|s| s.as_str())
            .unwrap_or("<unset>"),
        merged_envs
            .get(ENV_ANTHROPIC_BASE_URL)
            .map(|s| s.as_str())
            .unwrap_or("<unset>")
    );

    if let Some(ref resolved) = resolved_model_env {
        apply_sensitive_model_env_fallback(
            &mut merged_envs,
            resolved,
            &explicitly_bound_model_env_keys,
        );
    }

    // 🔧 Windows：将 .cmd/.bat 等规范化为不弹窗的 node.exe + JS 形式（逻辑在 windows_launch 中）
    #[cfg(windows)]
    let (command_path, command_args) =
        normalize_windows_command_for_no_window(command_path, command_args);

    // 📋 打印完整的子进程环境变量（用于调试代理 URL 问题）
    info!(
        "[SACP] Subprocess environment variables ({} items):",
        merged_envs.len()
    );
    // 需要脱敏的环境变量 key 列表
    const SENSITIVE_ENV_KEYS: &[&str] = &[
        ENV_ANTHROPIC_API_KEY,
        ENV_OPENAI_API_KEY,
        ENV_CODEX_API_KEY,
        "ANTHROPIC_AUTH_TOKEN",
    ];
    let mut env_entries: Vec<_> = merged_envs.iter().collect();
    env_entries.sort_by_key(|(k, _)| *k);
    for (key, value) in &env_entries {
        if SENSITIVE_ENV_KEYS.contains(&key.as_str()) {
            // 脱敏：只显示前4个字符 + ***
            let masked = if value.len() > 4 {
                format!("{}***", &value[..4])
            } else {
                "***".to_string()
            };
            info!("[SACP]   {} = {}", key, masked);
        } else {
            info!("[SACP]   {} = {}", key, value);
        }
    }

    let full_command_line = format!("{} {}", command_path, command_args.join(" "));
    FinalizedLaunchEnv {
        command_path,
        command_args,
        merged_envs,
        full_command_line,
    }
}
