use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use agent_client_protocol::schema::v1::{PromptRequest, SessionId};
use anyhow::Result;
use shared_types::{ModelProviderConfig, ProjectAndAgentInfo};
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::super::lifecycle::AgentLifecycleGuard;
use super::super::model_env::ModelRuntimeEnvResolver;
use super::connection::{SacpConnectionParams, run_sacp_connection};
use super::launch_env::{finalize_subprocess_env, resolve_agent_command};
use super::launch_spawn::{spawn_agent_process, spawn_stderr_reader, wait_for_session_id};
use super::mcp::convert_context_servers_sacp;
use super::types::SacpLauncherConnectionInfo;
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

        // 解析 Agent 命令与基础环境（模型环境解析、agent_server 覆盖合并、
        // {PREFIX_WORKSPACE_DIR} 占位符渲染）
        let resolved_command = resolve_agent_command(
            self.model_env_resolver.as_ref(),
            model_provider.as_ref(),
            &start_config,
            service_uuid.as_deref(),
        )
        .await?;

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
        } else if !resolved_command
            .default_agent_config
            .context_servers
            .is_empty()
        {
            info!("[SACP] using config file MCP servers");
            convert_context_servers_sacp(&resolved_command.default_agent_config.context_servers)?
        } else {
            info!("[SACP] no config MCP servers");
            Vec::new()
        };

        // 确定子进程最终启动环境（平台分支、UUID 替换、敏感回退、环境打印）
        // 先取出模型 id（resume 后 session 模型引用同步的主源，源自
        // model_provider），resolved_command 随即被 move 进 finalize
        let session_model_id = resolved_command
            .resolved_model_env
            .as_ref()
            .map(|env| env.default_model.clone());
        let finalized_env = finalize_subprocess_env(
            resolved_command,
            &project_id,
            &project_path,
            &start_config,
            service_uuid.as_deref(),
        );

        // 构建并启动子进程（平台 cfg 分支集中于 launch_spawn）
        let spawned = spawn_agent_process(
            &finalized_env.command_path,
            &finalized_env.command_args,
            &finalized_env.merged_envs,
            &project_path,
            &finalized_env.full_command_line,
        )?;

        let child_pid = spawned.child_pid;

        // P0-2 接线: 进程启动成功,通知 listener
        if let Some(ref listener) = self.diagnostics_listener {
            listener.on_process_started(child_pid, &finalized_env.full_command_line);
        }

        // 🔥 立即启动 stderr 读取任务（在 session_id 等待之前）
        // 这样即使子进程在初始化阶段就退出，也能捕获 stderr 输出
        let (stderr_task_handle, stderr_output_shared) =
            spawn_stderr_reader(spawned.stderr, &cancel_token);

        // 创建 SACP transport
        let transport = agent_client_protocol::ByteStreams::new(
            spawned.stdin.compat_write(),
            spawned.stdout.compat(),
        );

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
        let spawn_command_line = finalized_env.full_command_line.clone();
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
            // resume 后同步 session 模型引用：裸模型 id（源自 model_provider），
            // 值形态（provider 前缀）在协议现场从 agent 的 configOptions 响应匹配
            let session_model_id = session_model_id.clone();
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
                session_model_id,
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
                if let Some(tx) = connection_failed_tx.take()
                    && let Err(send_err) = tx.send(format!("{}", e))
                {
                    warn!(
                        "[SACP] connection_failed_tx send failed (receiver dropped), error was: {}",
                        send_err
                    );
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
        let session_id = wait_for_session_id(
            &project_id,
            session_create_timeout_secs,
            session_id_rx,
            connection_failed_rx,
            &connection_task_handle,
            &stderr_output_shared,
            &connection_error_shared,
            &finalized_env.full_command_line,
            child_pid,
        )
        .await?;

        // stderr 任务已在子进程启动后立即创建（stderr_task_handle），无需重复创建

        // 创建生命周期守卫（带异常退出标志 + 诊断监听器）
        let lifecycle_guard = AgentLifecycleGuard::new_claude_full(
            crate::launcher::lifecycle::ClaudeProcessParams {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
                child_process: spawned.child,
                stderr_task: stderr_task_handle,
                cancel_token: cancel_token.clone(),
                shared_api_key_manager: None,
                project_uuid_map: None,
                service_uuid: None,
                abnormal_exit_flag: Some(abnormal_exit_flag),
                exit_detail: Some(exit_detail),
                diagnostics_listener: self.diagnostics_listener.clone(),
                process_command: finalized_env.command_path,
                process_args: finalized_env.command_args,
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
