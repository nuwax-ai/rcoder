use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use agent_client_protocol::schema::{PromptRequest, SessionId};
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
    render_model_template,
};
use super::mcp::convert_context_servers_sacp;
use super::process::take_stdio;
use super::types::{
    ENV_AGENT_PROJECT_ID, ENV_AGENT_WORKING_DIR, ENV_ANTHROPIC_API_KEY, ENV_ANTHROPIC_BASE_URL,
    ENV_CODEX_API_KEY, ENV_OPENAI_API_KEY, ENV_OPENAI_BASE_URL, SacpLauncherConnectionInfo,
};
use crate::acp::CancelNotificationRequestWrapper;
use crate::launcher::model_env;
use crate::traits::AgentStartConfig;
use crate::traits::session_notifier::SessionNotifier;
use crate::traits::session_registry::SessionRegistry;

/// Claude Code ACP Agent 启动器 (SACP 版本)
///
/// 使用 SACP 库的 Builder 模式和回调函数，无需 LocalSet。
pub struct SacpClaudeCodeLauncher<N: SessionNotifier> {
    /// 会话通知器
    notifier: Arc<N>,
    model_env_resolver: Arc<dyn ModelRuntimeEnvResolver>,
}

impl<N: SessionNotifier + 'static> SacpClaudeCodeLauncher<N> {
    /// 创建新的启动器
    pub fn new(notifier: Arc<N>) -> Self {
        Self::with_model_env_resolver(notifier, model_env::direct_model_runtime_env_resolver())
    }

    pub fn with_model_env_resolver(
        notifier: Arc<N>,
        model_env_resolver: Arc<dyn ModelRuntimeEnvResolver>,
    ) -> Self {
        Self {
            notifier,
            model_env_resolver,
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
            "[SACP] 🚀 LAUNCH FUNCTION CALLED: project_id={}, has_agent_server_override={}, service_uuid={:?}",
            project_id,
            start_config.agent_server_override.is_some(),
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
                for (_key, value) in env.iter_mut() {
                    render_model_template(value, resolved);
                }
                let bound_model_env_keys = apply_model_env_bindings(
                    &mut env,
                    &agent_server_override.model_env_bindings,
                    resolved,
                );
                debug!(
                    "🔧 [SACP] Replaced custom env var template, model={}",
                    resolved.default_model
                );
                info!(
                    "[SACP] Applied {} model env bindings",
                    bound_model_env_keys.len()
                );
                info!(
                    "🎯 [SACP] Using custom Agent: agent_id={}, command={} {:?}",
                    agent_server_override.get_agent_id(),
                    cmd,
                    args
                );
                (cmd, args, env, bound_model_env_keys)
            } else {
                if !agent_server_override.model_env_bindings.is_empty() {
                    warn!(
                        "[SACP] model_env_bindings configured but model_provider is missing; bindings were not applied"
                    );
                }
                info!(
                    "🎯 [SACP] Using custom Agent: agent_id={}, command={} {:?}",
                    agent_server_override.get_agent_id(),
                    cmd,
                    args
                );
                (cmd, args, env, HashSet::new())
            }
        } else {
            // 使用默认配置
            info!(
                "📋 [SACP] Using default Agent: {} {:?}",
                default_agent_config.command, default_agent_config.args
            );
            (
                default_agent_config.command.clone(),
                default_agent_config.args.clone(),
                default_agent_config.env.clone(),
                HashSet::new(),
            )
        };

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
            convert_context_servers_sacp(&default_agent_config.context_servers)
        } else {
            info!("📝 [SACP] no config MCP servers");
            Vec::new()
        };

        #[cfg(windows)]
        if let Some((resolved_program, resolved_args)) =
            resolve_windows_node_cli_command(&command_path, &command_args)
        {
            let entry = resolved_args.first().cloned().unwrap_or_default();
            info!(
                "[SACP] Windows direct node startup: {} -> {} {}",
                command_path, resolved_program, entry
            );
            command_path = resolved_program;
            command_args = resolved_args;
        }

        // 准备环境变量（在 base_env 基础上添加项目相关变量）
        let mut merged_envs = base_env;
        merged_envs.insert(
            ENV_AGENT_WORKING_DIR.to_string(),
            project_path.to_string_lossy().to_string(),
        );
        merged_envs.insert(ENV_AGENT_PROJECT_ID.to_string(), project_id.clone());

        ensure_subprocess_path_env(&mut merged_envs);

        // 🔍 调试：打印替换前的关键环境变量
        info!(
            "[SACP] 🔍 Before UUID replacement: OPENAI_BASE_URL={}, ANTHROPIC_BASE_URL={}, service_uuid={:?}",
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
            info!("[SACP] 🔍 Replacing {{SERVICE_UUID}} with: {}", uuid);
            for (_key, value) in merged_envs.iter_mut() {
                *value = value.replace("{SERVICE_UUID}", uuid);
            }
        } else {
            warn!("[SACP] ⚠️ service_uuid is None, UUID placeholder will NOT be replaced!");
        }

        // 🔍 调试：打印替换后的关键环境变量
        info!(
            "[SACP] 🔍 After UUID replacement: OPENAI_BASE_URL={}, ANTHROPIC_BASE_URL={}",
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
            "[SACP] 📋 Subprocess environment variables ({} items):",
            merged_envs.len()
        );
        // 需要脱敏的环境变量 key 列表
        const SENSITIVE_ENV_KEYS: &[&str] = &[
            ENV_ANTHROPIC_API_KEY,
            ENV_OPENAI_API_KEY,
            ENV_CODEX_API_KEY,
            "ANTHROPIC_AUTH_TOKEN",
        ];
        let mut env_keys: Vec<_> = merged_envs.keys().collect();
        env_keys.sort();
        for key in &env_keys {
            let value = merged_envs.get(*key).unwrap();
            if SENSITIVE_ENV_KEYS.contains(&key.as_str()) {
                // 脱敏：只显示前4个字符 + ***
                let masked = if value.len() > 4 {
                    format!("{}***", &value[..4])
                } else {
                    "***".to_string()
                };
                info!("[SACP] 📋   {} = {}", key, masked);
            } else {
                info!("[SACP] 📋   {} = {}", key, value);
            }
        }

        // 启动子进程（使用进程组/Job Object 来管理整个进程树）
        // Unix: ProcessGroup::leader() 创建进程组，确保能够清理所有孙进程
        // Windows: JobObject 管理进程树
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
            .context("[SACP] Failed to start claude-code-acp-ts subprocess")?;

        #[cfg(windows)]
        let mut child = cmd_wrap
            .wrap(CreationFlags(PROCESS_CREATION_FLAGS(CREATE_NO_WINDOW_FLAG)))
            .wrap(JobObject)
            .spawn()
            .context("[SACP] Failed to start claude-code-acp-ts subprocess")?;

        #[cfg(not(any(unix, windows)))]
        compile_error!("neither unix nor windows");

        let child_pid = child.id().unwrap_or(0);
        info!(
            "[SACP] Claude Code ACP child process already started, PID: {}",
            child_pid
        );

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
                                warn!("[SACP] Claude Code Agent stderr: {}", line.trim());
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

        // 共享的 session_id，用于连接失败时发送错误通知
        let session_id_shared = Arc::new(std::sync::Mutex::new(None::<String>));

        // 共享的连接错误信息，用于 "channel dropped" 时传播真实错误原因
        let connection_error_shared = Arc::new(std::sync::Mutex::new(None::<String>));

        // 克隆用于闭包
        let project_path_clone = project_path.clone();
        let project_id_clone = project_id.clone();
        let cancel_token_clone = cancel_token.clone();
        let notifier_clone = self.notifier.clone();
        let abnormal_exit_flag_clone = abnormal_exit_flag.clone();
        let session_id_shared_clone = session_id_shared.clone();
        let connection_error_clone = connection_error_shared.clone();
        let error_notifier = self.notifier.clone();

        // 🔥 连接失败通知通道：连接任务失败时立即通知，不等 60 秒超时
        let (connection_failed_tx, connection_failed_rx) =
            tokio::sync::oneshot::channel::<String>();
        let mut connection_failed_tx = Some(connection_failed_tx);

        // 保存 command_path 用于超时日志
        let command_path_for_log = command_path.clone();

        // 🔥 使用标准 tokio::spawn（无需 LocalSet！）
        // 保存 JoinHandle 用于超时时取消子任务
        let spawn_project_id = project_id.clone();
        let connection_task_handle = tokio::spawn(async move {
            info!(
                "[SACP] 🚀 Spawned ACP connection task, project_id={}",
                spawn_project_id
            );
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
                abnormal_exit_flag: abnormal_exit_flag_clone,
                session_id_shared: session_id_shared_clone,
                connection_failed_tx: connection_failed_tx.take(),
                child_pid,
            };
            let result = run_sacp_connection(transport, params).await;

            match &result {
                Ok(_) => info!(
                    "[SACP] ✅ ACP connection task completed successfully, project_id={}",
                    spawn_project_id
                ),
                Err(e) => error!(
                    "[SACP] ❌ ACP connection task failed: {}, project_id={}",
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
                    let error = agent_client_protocol::schema::Error::new(
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

        // 等待会话 ID（60 秒超时），同时监听连接失败
        info!(
            "[SACP] Waiting for session_id from ACP agent, project_id={}, timeout=60s",
            project_id
        );
        let session_id = match tokio::time::timeout(
            std::time::Duration::from_secs(60),
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
                    "[SACP] ⏰ Agent initialization timeout (60s), project_id={}, command={}, child_pid={}, stderr={}",
                    project_id, command_path_for_log, child_pid, stderr_info
                );
                // 超时后取消 spawned 任务，避免子进程泄漏
                connection_task_handle.abort();
                // kill 子进程（使用进程组 kill 清理所有孙进程）
                #[cfg(unix)]
                {
                    use nix::sys::signal::{Signal, killpg};
                    use nix::unistd::Pid;
                    if child_pid > 0 {
                        let pgid = Pid::from_raw(-(child_pid as i32));
                        match killpg(pgid, Signal::SIGKILL) {
                            Ok(_) => warn!(
                                "[SACP] Killed process group (SIGKILL) for child_pid={}, project_id={}",
                                child_pid, project_id
                            ),
                            Err(e) => error!(
                                "[SACP] Failed to kill process group for child_pid={}: {}, project_id={}",
                                child_pid, e, project_id
                            ),
                        }
                    }
                }
                return Err(anyhow::anyhow!(
                    "{}: agent initialization timeout (60s){}",
                    error_codes::get_i18n_message_default("error.agent_init_timeout"),
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

        // 创建生命周期守卫（带异常退出标志）
        let lifecycle_guard = AgentLifecycleGuard::new_claude_with_abnormal_flag(
            project_id.clone(),
            session_id.clone(),
            child,
            stderr_task_handle,
            cancel_token.clone(),
            abnormal_exit_flag,
        );

        Ok(SacpLauncherConnectionInfo {
            session_id,
            prompt_tx,
            cancel_tx,
            lifecycle_guard: Arc::new(lifecycle_guard),
        })
    }
}
