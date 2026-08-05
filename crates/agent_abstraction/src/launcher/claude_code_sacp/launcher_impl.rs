use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use agent_client_protocol::schema::v1::{PromptRequest, SessionId};
use anyhow::{Context, Result};
use process_wrap::tokio::CommandWrap;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
#[cfg(windows)]
use process_wrap::tokio::{CreationFlags, JobObject};
use shared_types::{ModelProviderConfig, ProjectAndAgentInfo, error_codes};
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
#[cfg(windows)]
use windows::Win32::System::Threading::PROCESS_CREATION_FLAGS;

use super::super::lifecycle::AgentLifecycleGuard;
use super::super::model_env::ModelRuntimeEnvResolver;
#[cfg(windows)]
use super::super::windows_launch::{
    CREATE_NO_WINDOW_FLAG, normalize_windows_command_for_no_window,
    resolve_windows_node_cli_command,
};
use super::config::load_sacp_agent_config_with_resolver;
use super::connection::{SacpConnectionParams, run_sacp_connection};
use super::env::{
    apply_model_env_bindings, apply_sensitive_model_env_fallback, ensure_subprocess_path_env,
    render_model_template, render_prefix_workspace_dir,
};
use super::mcp::convert_context_servers_sacp;
use super::process::take_stdio;
use super::types::{
    ENV_AGENT_PROJECT_ID, ENV_AGENT_WORKING_DIR, ENV_ANTHROPIC_API_KEY, ENV_ANTHROPIC_BASE_URL,
    ENV_CODEX_API_KEY, ENV_OPENAI_API_KEY, ENV_OPENAI_BASE_URL, ENV_OPENCODE_PERMISSION,
    SacpLauncherConnectionInfo,
};
use crate::acp::CancelNotificationRequestWrapper;
use crate::diagnostics::DiagnosticsListener;
use crate::launcher::model_env;
use crate::traits::session_notifier::SessionNotifier;
use crate::traits::session_registry::SessionRegistry;
use crate::traits::{AgentStartConfig, PermissionRequestHandler, YoloPermissionRequestHandler};

/// Claude Code ACP Agent 启动器 (SACP 版本)
///
/// 使用 SACP 库的 Builder 模式和回调函数，无需 LocalSet。
pub struct SacpClaudeCodeLauncher<N: SessionNotifier> {
    /// 会话通知器
    notifier: Arc<N>,
    model_env_resolver: Arc<dyn ModelRuntimeEnvResolver>,
    permission_handler: Arc<dyn PermissionRequestHandler>,
    /// 进程诊断监听器（可选，注入自 AcpClientBuilder）
    diagnostics_listener: Option<Arc<dyn DiagnosticsListener>>,
}

impl<N: SessionNotifier + 'static> SacpClaudeCodeLauncher<N> {
    /// 创建新的启动器
    pub fn new(notifier: Arc<N>) -> Self {
        Self::with_diagnostics_listener(
            notifier,
            model_env::direct_model_runtime_env_resolver(),
            Arc::new(YoloPermissionRequestHandler),
            None,
        )
    }

    pub fn with_model_env_resolver(
        notifier: Arc<N>,
        model_env_resolver: Arc<dyn ModelRuntimeEnvResolver>,
        permission_handler: Arc<dyn PermissionRequestHandler>,
    ) -> Self {
        Self::with_diagnostics_listener(notifier, model_env_resolver, permission_handler, None)
    }

    /// 注入诊断监听器
    pub fn with_diagnostics_listener(
        notifier: Arc<N>,
        model_env_resolver: Arc<dyn ModelRuntimeEnvResolver>,
        permission_handler: Arc<dyn PermissionRequestHandler>,
        diagnostics_listener: Option<Arc<dyn DiagnosticsListener>>,
    ) -> Self {
        Self {
            notifier,
            model_env_resolver,
            permission_handler,
            diagnostics_listener,
        }
    }

    /// 启动 Claude Code ACP Agent 服务
    ///
    /// 使用 SACP 库的 Builder 模式，支持标准 tokio::spawn
    pub async fn launch<R: SessionRegistry + 'static>(
        &self,
        project_id: String,
        project_path: PathBuf,
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
        _registry: Arc<R>,
        service_uuid: Option<String>,
    ) -> Result<SacpLauncherConnectionInfo>
    where
        R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
    {
        info!(
            "[SACP] LAUNCH FUNCTION CALLED: project_id={}, has_agent_server_override={}, has_model_provider={}, service_uuid={:?}",
            project_id,
            start_config.agent_server_override.is_some(),
            model_provider.is_some(),
            service_uuid
        );

        // 从配置加载默认 Agent 参数
        let resolved_model_env = model_provider
            .as_ref()
            .map(|provider| {
                self.model_env_resolver
                    .resolve(provider, service_uuid.as_deref())
            })
            .transpose()?;
        let default_agent_config = load_sacp_agent_config_with_resolver(
            model_provider.as_ref(),
            &start_config.service_type,
            self.model_env_resolver.as_ref(),
            service_uuid.as_deref(),
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
        let command_path = resolved_command;
        let command_args = resolved_args;
        let base_env = resolved_env;

        // 创建通道（使用有界通道防止 OOM）
        // 容量由常量定义，足够处理突发请求，同时提供背压保护
        let (cancel_tx, cancel_rx) = mpsc::channel::<CancelNotificationRequestWrapper>(
            shared_types::AGENT_CANCEL_CHANNEL_CAPACITY,
        );
        let (prompt_tx, prompt_rx) =
            mpsc::channel::<PromptRequest>(shared_types::AGENT_PROMPT_CHANNEL_CAPACITY);
        let (session_id_tx, session_id_rx) = tokio::sync::oneshot::channel::<SessionId>();

        // 创建 CancellationToken
        let cancel_token = CancellationToken::new();

        info!(
            "[SACP] projectworkdirectory: {}",
            &project_path.to_string_lossy()
        );

        // 准备 MCP 服务器
        let mcp_servers = if start_config.has_mcp_servers() {
            info!("[SACP] using AgentStartConfig MCP servers");
            start_config.mcp_servers.clone()
        } else if !default_agent_config.context_servers.is_empty() {
            info!("[SACP] using config file MCP servers");
            convert_context_servers_sacp(&default_agent_config.context_servers)?
        } else {
            info!("[SACP] no config MCP servers");
            Vec::new()
        };

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
        merged_envs.insert(ENV_AGENT_PROJECT_ID.to_string(), project_id.clone());

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
        if let Some(ref uuid) = service_uuid {
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

        // 启动子进程（使用进程组/Job Object 来管理整个进程树）
        // Unix: ProcessGroup::leader() 创建进程组，确保能够清理所有孙进程
        // Windows: JobObject 管理进程树
        let full_command_line = format!("{} {}", command_path, command_args.join(" "));
        info!(
            "[SACP] Spawning subprocess: cmd=[{}], cwd={}",
            full_command_line,
            project_path.display()
        );
        let mut cmd_wrap = CommandWrap::with_new(&command_path, |cmd| {
            cmd.args(&command_args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .current_dir(&project_path);
            cmd.envs(&merged_envs);
        });

        #[cfg(unix)]
        let mut child = cmd_wrap
            .wrap(ProcessGroup::leader())
            .spawn()
            .context("[SACP] Failed to start ACP subprocess")?;

        #[cfg(windows)]
        let mut child = cmd_wrap
            .wrap(CreationFlags(PROCESS_CREATION_FLAGS(CREATE_NO_WINDOW_FLAG)))
            .wrap(JobObject)
            .spawn()
            .context("[SACP] Failed to start ACP subprocess")?;

        #[cfg(not(any(unix, windows)))]
        compile_error!("neither unix nor windows");

        let child_pid = child.id().unwrap_or(0);
        info!(
            "[SACP] Claude Code ACP child process already started, PID: {}",
            child_pid
        );

        // P0-2 接线: 进程启动成功,通知 listener
        if let Some(ref listener) = self.diagnostics_listener {
            listener.on_process_started(child_pid, &full_command_line);
        }

        // 获取 stdio 句柄（process_wrap 使用方法访问 stdio）
        let stdin = take_stdio(child.stdin(), "stdin")?;
        let stdout = take_stdio(child.stdout(), "stdout")?;
        let stderr = take_stdio(child.stderr(), "stderr")?;

        // 🔥 立即启动 stderr 读取任务（在 session_id 等待之前）
        // 这样即使子进程在初始化阶段就退出，也能捕获 stderr 输出
        let cancel_token_for_stderr = cancel_token.clone();
        let stderr_output_shared = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let stderr_output_clone = stderr_output_shared.clone();
        let stderr_task_handle: tokio::task::JoinHandle<()> = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();

            loop {
                tokio::select! {
                    biased; // 优先检查取消信号

                    _ = cancel_token_for_stderr.cancelled() => {
                        debug!("[SACP] stderr cancel received");
                        break;
                    }
                    result = lines.next_line() => {
                        match result {
                            Ok(Some(line)) if !line.trim().is_empty() => {
                                warn!("[SACP] ACP Agent stderr: {}", line.trim());
                                // 存储 stderr 输出，用于错误传播
                                if let Ok(mut buf) = stderr_output_clone.lock() {
                                    buf.push(line.trim().to_string());
                                    // 限制最多存储 20 行，避免内存膨胀
                                    if buf.len() > 20 {
                                        buf.remove(0);
                                    }
                                }
                            }
                            Ok(Some(_)) => {} // 空行，忽略
                            Ok(None) => break, // EOF
                            Err(e) => {
                                error!("[SACP] read stderr failed: {}", e);
                                break;
                            }
                        }
                    }
                }
            }
        });

        // 创建 SACP transport
        let transport =
            agent_client_protocol::ByteStreams::new(stdin.compat_write(), stdout.compat());

        // 🔥 新增：创建共享的异常退出标志
        // 此标志在 reaper_task 检测到子进程异常退出时设置为 true
        // SACP 连接层可以检测此标志并发送相应的错误通知
        let abnormal_exit_flag = Arc::new(AtomicBool::new(false));

        // 🔥 新增：详细的退出信息（signal、exit_code），用于生成更有意义的错误消息
        let exit_detail = Arc::new(tokio::sync::Mutex::new(
            None::<crate::launcher::lifecycle::ExitDetail>,
        ));

        // 共享的 session_id，用于连接失败时发送错误通知
        let session_id_shared = Arc::new(std::sync::Mutex::new(None::<String>));

        // 共享的连接错误信息，用于 "channel dropped" 时传播真实错误原因
        let connection_error_shared = Arc::new(std::sync::Mutex::new(None::<String>));

        // 克隆用于闭包
        let project_path_clone = project_path.clone();
        let project_id_clone = project_id.clone();
        let cancel_token_clone = cancel_token.clone();
        let notifier_clone = self.notifier.clone();
        let permission_handler_clone = self.permission_handler.clone();
        let abnormal_exit_flag_clone = abnormal_exit_flag.clone();
        let exit_detail_clone = exit_detail.clone();
        let session_id_shared_clone = session_id_shared.clone();
        let connection_error_clone = connection_error_shared.clone();
        let error_notifier = self.notifier.clone();

        // 🔥 连接失败通知通道：连接任务失败时立即通知，不等 60 秒超时
        let (connection_failed_tx, connection_failed_rx) =
            tokio::sync::oneshot::channel::<String>();
        let mut connection_failed_tx = Some(connection_failed_tx);

        // command_path 信息现在通过 full_command_line 传递

        // 🔥 使用标准 tokio::spawn（无需 LocalSet！）
        // 保存 JoinHandle 用于超时时取消子任务
        let spawn_project_id = project_id.clone();
        let spawn_command_line = full_command_line.clone();
        let diagnostics_listener_for_task = self.diagnostics_listener.clone();
        // 🔥 提前取出 session 创建超时：start_config 随即被 move 进 spawn task，之后不可读。
        // 值来自 AgentStartConfig（由 GrpcTimeoutConfig.acp_session_create_timeout_secs 注入），默认 60s。
        let session_create_timeout_secs =
            start_config.acp_session_create_timeout_secs.unwrap_or(60);
        let connection_task_handle = tokio::spawn(async move {
            info!(
                "[SACP] Spawned ACP connection task, project_id={}",
                spawn_project_id
            );
            let command_line_clone = spawn_command_line;
            let params = SacpConnectionParams {
                project_path: project_path_clone,
                project_id: project_id_clone.clone(),
                mcp_servers,
                start_config,
                session_id_tx,
                prompt_rx,
                cancel_rx,
                cancel_token: cancel_token_clone,
                notifier: notifier_clone,
                permission_handler: permission_handler_clone,
                abnormal_exit_flag: abnormal_exit_flag_clone,
                exit_detail: exit_detail_clone,
                session_id_shared: session_id_shared_clone,
                connection_failed_tx: connection_failed_tx.take(),
                child_pid,
                command_line: command_line_clone,
                diagnostics_listener: diagnostics_listener_for_task,
            };
            let result = run_sacp_connection(transport, params).await;

            match &result {
                Ok(_) => info!(
                    "[SACP] ACP connection task completed successfully, project_id={}",
                    spawn_project_id
                ),
                Err(e) => error!(
                    "[SACP] ACP connection task failed: {}, project_id={}",
                    e, spawn_project_id
                ),
            }

            if let Err(e) = result {
                error!("[SACP] Claude Code ACP Agent connection failed: {}", e);

                // 存储连接错误到共享状态，供 "channel dropped" 时使用
                if let Ok(mut guard) = connection_error_clone.lock() {
                    *guard = Some(format!("{}", e));
                }

                // 🔥 立即通知外层连接失败，避免等待 60 秒超时
                if let Some(tx) = connection_failed_tx.take() {
                    let _ = tx.send(format!("{}", e));
                }

                // 🔥 关键修复：连接失败时发送错误通知到 SSE 流
                // 只有在 session_id 已经初始化的情况下才能发送（连接建立后才会有 session_id）
                let session_id = session_id_shared
                    .lock()
                    .ok()
                    .and_then(|guard| guard.clone());

                if let Some(session_id) = session_id {
                    warn!(
                        "[SACP] Sending error notification to SSE stream: project_id={}, session_id={}",
                        project_id_clone, session_id
                    );
                    let error = agent_client_protocol::schema::v1::Error::new(
                        1001,
                        format!("ACP connection failed: {}", e),
                    );
                    if let Err(notify_err) = error_notifier
                        .notify_prompt_error(&project_id_clone, &session_id, error, None)
                        .await
                    {
                        error!("[SACP] Failed to send error notification: {:?}", notify_err);
                    }
                } else {
                    debug!(
                        "[SACP] session_id not yet available, skipping error notification: project_id={}",
                        project_id_clone
                    );
                }
            }
        });

        // 等待会话 ID（超时取自 start_config / GrpcTimeoutConfig，默认 60s），同时监听连接失败
        info!(
            "[SACP] Waiting for session_id from ACP agent, project_id={}, timeout={}s",
            project_id, session_create_timeout_secs
        );
        let session_id = match tokio::time::timeout(
            std::time::Duration::from_secs(session_create_timeout_secs),
            async {
                tokio::select! {
                    result = session_id_rx => {
                        match result {
                            Ok(sid) => Ok(Ok(sid)),
                            Err(e) => Ok(Err(anyhow::anyhow!("channel dropped: {}", e))),
                        }
                    }
                    failed = connection_failed_rx => {
                        match failed {
                            Ok(err_msg) => Err(anyhow::anyhow!("{}", err_msg)),
                            Err(_) => Ok(Err(anyhow::anyhow!("connection ended without session_id or error"))),
                        }
                    }
                }
            },
        )
        .await
        {
            Ok(Ok(Ok(session_id))) => {
                info!(
                    "[SACP] Received session_id from ACP agent: {}, project_id={}",
                    session_id, project_id
                );
                session_id
            }
            Err(_timeout_elapsed) => {
                // 60 秒超时，连接任务仍在运行
                let stderr_info = stderr_output_shared.lock().ok()
                    .map(|buf| buf.join("\n"))
                    .filter(|s| !s.is_empty())
                    .map(|s| format!("; stderr: {}", s))
                    .unwrap_or_default();
                error!(
                    "[SACP] Agent initialization timeout ({}s), project_id={}, command=[{}], child_pid={}, stderr={}",
                    session_create_timeout_secs, project_id, full_command_line, child_pid, stderr_info
                );
                // 超时后取消 spawned 任务，避免子进程泄漏
                connection_task_handle.abort();
                // kill 子进程（使用进程组 kill 清理所有孙进程）
                #[cfg(unix)]
                {
                    use nix::errno::Errno;
                    use nix::sys::signal::{Signal, kill};
                    use nix::unistd::Pid;
                    if child_pid > 1 {
                        // kill(2) 的负数 pid 表示「整个进程组」；子进程以 ProcessGroup::leader() 启动，pgid == child_pid，
                        // 故 -child_pid 能杀掉 claude-code-acp-ts 及其所有 MCP 孙进程。
                        // ⚠️ 绝不能用 libc killpg 并预先取负：killpg 期望正数 pgrp（内部自取负），负数会直接 EINVAL。
                        // 与 lifecycle.rs 的正常关闭路径保持一致。
                        // ⚠️ child_pid==1 时 kill(-1) 语义是「所有进程组」且 PID 1 信号被内核忽略，必须跳过。
                        let target = Pid::from_raw(-(child_pid as i32));
                        match kill(target, Signal::SIGKILL) {
                            Ok(_) => warn!(
                                "[SACP] Killed process group (SIGKILL) for child_pid={}, project_id={}",
                                child_pid, project_id
                            ),
                            Err(Errno::ESRCH) => debug!(
                                "[SACP] Process group already exited: child_pid={}, project_id={}",
                                child_pid, project_id
                            ),
                            Err(e) => error!(
                                "[SACP] Failed to kill process group for child_pid={}: {}, project_id={}",
                                child_pid, e, project_id
                            ),
                        }
                    } else if child_pid == 1 {
                        warn!(
                            "[SACP] child_pid==1（容器 PID 1），跳过进程组 kill，依赖 init 收割: project_id={}",
                            project_id
                        );
                    }
                }
                return Err(anyhow::anyhow!(
                    "{}: agent initialization timeout ({}s){}",
                    error_codes::get_i18n_message_default("error.agent_init_timeout"),
                    session_create_timeout_secs,
                    stderr_info
                ));
            }
            Ok(Err(e)) => {
                // 连接任务主动报告了失败，立即返回
                let err_str = e.to_string();
                let stderr_info = stderr_output_shared.lock().ok()
                    .map(|buf| buf.join("\n"))
                    .filter(|s| !s.is_empty())
                    .map(|s| format!("; stderr: {}", s))
                    .unwrap_or_default();
                let clean_msg = err_str
                    .strip_prefix("connection failed: ")
                    .unwrap_or(&err_str);
                error!(
                    "[SACP] Agent connection failed early: project_id={}, error={}, stderr={}",
                    project_id, err_str, stderr_info
                );
                return Err(anyhow::anyhow!(
                    "Agent process failed: {}{}",
                    clean_msg,
                    stderr_info
                ));
            }
            Ok(Ok(Err(e))) => {
                // channel dropped — 读取连接任务的实际错误原因
                let connection_error = connection_error_shared.lock().ok()
                    .and_then(|guard| guard.clone())
                    .unwrap_or_else(|| "unknown error".to_string());
                // 读取 stderr 输出
                let stderr_info = stderr_output_shared.lock().ok()
                    .map(|buf| buf.join("\n"))
                    .filter(|s| !s.is_empty())
                    .map(|s| format!("; stderr: {}", s))
                    .unwrap_or_default();
                error!(
                    "[SACP] session_id channel dropped (connection task failed): recv_error={}, actual_error={}, project_id={}",
                    e, connection_error, project_id
                );
                // 连接任务已自行结束，无需 abort
                return Err(anyhow::anyhow!(
                    "{}: {}{}",
                    error_codes::get_i18n_message_default("error.agent_init_timeout"),
                    connection_error,
                    stderr_info
                ));
            }
        };

        info!(
            "[SACP] Claude Code ACP Agent service started successfully, session ID: {}",
            session_id
        );

        // stderr 任务已在子进程启动后立即创建（stderr_task_handle），无需重复创建

        // 创建生命周期守卫（带异常退出标志 + 诊断监听器）
        let lifecycle_guard = AgentLifecycleGuard::new_claude_full(
            crate::launcher::lifecycle::ClaudeProcessParams {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
                child_process: child,
                stderr_task: stderr_task_handle,
                cancel_token: cancel_token.clone(),
                shared_api_key_manager: None,
                project_uuid_map: None,
                service_uuid: None,
                abnormal_exit_flag: Some(abnormal_exit_flag),
                exit_detail: Some(exit_detail),
                diagnostics_listener: self.diagnostics_listener.clone(),
                process_command: command_path,
                process_args: command_args,
                working_dir: project_path,
            },
        )?;

        Ok(SacpLauncherConnectionInfo {
            session_id,
            prompt_tx,
            cancel_tx,
            lifecycle_guard: Arc::new(lifecycle_guard),
        })
    }
}
