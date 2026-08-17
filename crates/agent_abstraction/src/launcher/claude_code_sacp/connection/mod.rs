//! SACP 连接编排：连接建立 + 消息循环骨架
//!
//! `run_sacp_connection` 只保留 Builder 编排骨架，具体阶段拆分至：
//! - `setup`: 连接初始化、会话创建/加载、ask 模式设置
//! - `message_loop`: prompt/cancel 消息主循环
//! - `notification_handlers`: SessionNotification 等消息类型处理

mod message_loop;
mod notification_handlers;
mod setup;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use agent_client_protocol::schema::v1::{
    McpServer, PromptRequest, RequestPermissionRequest, RequestPermissionResponse, SessionId,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Responder};
use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::acp::CancelNotificationRequestWrapper;
use crate::diagnostics::DiagnosticsListener;
use crate::traits::session_notifier::SessionNotifier;
use crate::traits::{AgentStartConfig, PermissionRequestContext, PermissionRequestHandler};
use shared_types::error_codes;

/// SACP 连接参数（封装 run_sacp_connection 的参数）
pub(crate) struct SacpConnectionParams<N: SessionNotifier> {
    pub(crate) project_path: PathBuf,
    pub(crate) project_id: String,
    pub(crate) mcp_servers: Vec<McpServer>,
    pub(crate) start_config: AgentStartConfig,
    pub(crate) session_id_tx: tokio::sync::oneshot::Sender<SessionId>,
    pub(crate) prompt_rx: mpsc::Receiver<PromptRequest>,
    pub(crate) cancel_rx: mpsc::Receiver<CancelNotificationRequestWrapper>,
    pub(crate) cancel_token: CancellationToken,
    pub(crate) notifier: Arc<N>,
    pub(crate) permission_handler: Arc<dyn PermissionRequestHandler>,
    /// 🔥 新增：共享的异常退出标志（子进程异常退出时设置为 true）
    pub(crate) abnormal_exit_flag: Arc<AtomicBool>,
    /// 🔥 新增：详细的退出信息（signal、exit_code），用于生成更有意义的错误消息
    pub(crate) exit_detail: Arc<tokio::sync::Mutex<Option<crate::launcher::lifecycle::ExitDetail>>>,
    /// 共享的 session_id，用于连接失败时发送错误通知
    /// 在 connect_with 内部初始化完成后设置，供外部错误处理使用
    pub(crate) session_id_shared: Arc<std::sync::Mutex<Option<String>>>,
    /// 🔥 连接失败通知通道：内部失败时立即通知外层，避免等待超时
    pub(crate) connection_failed_tx: Option<tokio::sync::oneshot::Sender<String>>,
    /// 子进程 PID，用于 waitpid 检测子进程退出
    pub(crate) child_pid: u32,
    /// 子进程命令行（用于错误诊断）
    pub(crate) command_line: String,
    /// 进程诊断监听器（可选，注入自 AcpClientBuilder）
    pub(crate) diagnostics_listener: Option<Arc<dyn DiagnosticsListener>>,
}

/// 运行 SACP 连接
///
/// 使用 SACP 的 Builder 模式建立连接并处理消息
pub(crate) async fn run_sacp_connection<N: SessionNotifier + 'static>(
    transport: agent_client_protocol::ByteStreams<
        tokio_util::compat::Compat<tokio::process::ChildStdin>,
        tokio_util::compat::Compat<tokio::process::ChildStdout>,
    >,
    params: SacpConnectionParams<N>,
) -> Result<()> {
    // 解构参数
    let SacpConnectionParams {
        project_path,
        project_id,
        mcp_servers,
        start_config,
        session_id_tx,
        prompt_rx,
        cancel_rx,
        cancel_token,
        notifier,
        permission_handler,
        abnormal_exit_flag,
        exit_detail,
        session_id_shared,
        mut connection_failed_tx,
        child_pid,
        command_line,
        diagnostics_listener,
    } = params;

    // resume 窗口标志：session/load 期间为 true（通知 handler 据此过滤历史重放）
    let resuming = Arc::new(AtomicBool::new(false));
    // 克隆变量供 handlers 使用
    let notifier_for_handlers = notifier.clone();
    let project_id_for_handlers = project_id.clone();
    let permission_handler_for_request = permission_handler.clone();
    let permission_context = PermissionRequestContext {
        project_id: project_id.clone(),
        user_id: start_config.user_id.clone(),
        agent_mode: start_config.agent_mode,
        service_type: start_config.service_type.clone(),
        request_id: None,
        tool_approval_rules: start_config.tool_approval_rules.clone(),
    };
    // 克隆 notifier 和 project_id 供 prompt 结束通知使用
    let notifier_for_prompt_end = notifier.clone();
    let project_id_for_prompt_end = project_id.clone();

    // 使用 SACP Builder 模式
    Client.builder()
        .name("rcoder-agent-runner-sacp")
        // 处理 SessionNotification 通知（使用 dispatch 方式，优雅处理未知消息类型）
        .on_receive_dispatch(
            {
                let notifier = notifier_for_handlers.clone();
                let project_id = project_id_for_handlers.clone();
                let resuming = resuming.clone();
                async move |dispatch: Dispatch, _cx: ConnectionTo<Agent>| {
                    notification_handlers::handle_incoming_dispatch(
                        dispatch,
                        notifier.clone(),
                        project_id.clone(),
                        resuming.clone(),
                    )
                    .await
                }
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        // 处理 RequestPermission
        .on_receive_request(
            {
                let permission_handler = permission_handler_for_request.clone();
                let context = permission_context.clone();
                move |request: RequestPermissionRequest,
                      responder: Responder<RequestPermissionResponse>,
                      _cx: ConnectionTo<Agent>| {
                    let permission_handler = permission_handler.clone();
                    let context = context.clone();
                    async move {
                        debug!("[SACP] permission request: {:?}", request);
                        permission_handler
                            .handle_permission_request(context, request, responder)
                            .await
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // 主连接逻辑
        .connect_with(transport, move |cx: ConnectionTo<Agent>| {
            let project_path = project_path.clone();
            let mcp_servers = mcp_servers.clone();
            let start_config = start_config.clone();
            let notifier_for_prompt = notifier_for_prompt_end.clone();
            let project_id_for_prompt = project_id_for_prompt_end.clone();
            let abnormal_exit_flag = abnormal_exit_flag.clone();
            let session_id_shared = session_id_shared.clone();
            let resuming = resuming.clone();

            async move {
                // 1. 初始化连接（INIT_TIMEOUT_SECS 秒超时）
                setup::initialize_connection(
                    &cx,
                    &project_id,
                    &cancel_token,
                    &command_line,
                    &abnormal_exit_flag,
                    child_pid,
                    &mut connection_failed_tx,
                )
                .await?;

                // 2+3. 构建会话 meta 并创建/加载会话
                let session_id = setup::create_or_load_session(
                    &cx,
                    &project_id,
                    project_path,
                    mcp_servers,
                    &start_config,
                    &resuming,
                )
                .await?;

                info!(
                    "[SACP] ACP session ready, session_id={}",
                    session_id
                );

                // 🆕 当 agent_mode=ask 时，通过 ACP 协议设置 session mode
                setup::apply_ask_session_mode(&cx, &start_config, &command_line, &session_id)
                    .await;

                // 发送会话 ID 到主任务
                if session_id_tx.send(session_id.clone()).is_err() {
                    error!("[SACP] unable to send session ID");
                    return Err(agent_client_protocol::Error::new(
                        1001,
                        error_codes::get_i18n_message_default("error.sacp_session_id_send_failed"),
                    ));
                }

                // 同步设置共享 session_id，供连接失败时的错误通知使用
                if let Ok(mut guard) = session_id_shared.lock() {
                    *guard = Some(session_id.to_string());
                }

                // P0-2 接线: ACP 握手成功,通知 listener
                if let Some(ref listener) = diagnostics_listener {
                    listener.on_acp_initialized(&session_id.to_string());
                }

                // 4. 处理 Prompt 和 Cancel 请求
                info!(
                    "[SACP] Step 4/4: Entering prompt processing loop, project_id={}, session_id={}",
                    project_id, session_id
                );
                message_loop::run_message_loop(
                    &cx,
                    project_id_for_prompt,
                    session_id,
                    notifier_for_prompt,
                    prompt_rx,
                    cancel_rx,
                    cancel_token,
                    abnormal_exit_flag,
                    exit_detail,
                )
                .await;

                Ok(())
            }
        })
        .await?;

    Ok(())
}
