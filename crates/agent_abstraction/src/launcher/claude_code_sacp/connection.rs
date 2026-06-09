use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_client_protocol::schema::{
    CancelNotification, InitializeRequest, LoadSessionRequest, McpServer, NewSessionRequest,
    PromptRequest, RequestPermissionRequest, RequestPermissionResponse, SessionId,
    SessionNotification, SetSessionModeRequest,
};
use agent_client_protocol::{
    Agent, Client, ConnectionTo, Dispatch, Handled, JsonRpcMessage, Responder,
};
use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::types::VERSION;
use crate::acp::CancelNotificationRequestWrapper;
use crate::diagnostics::DiagnosticsListener;
use crate::traits::session_notifier::SessionNotifier;
use crate::traits::{AgentStartConfig, PermissionRequestContext, PermissionRequestHandler};
use shared_types::error_codes;

/// InitializeRequest 超时时间（秒）
const INIT_TIMEOUT_SECS: u64 = 50;

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
        mut prompt_rx,
        mut cancel_rx,
        cancel_token,
        notifier,
        permission_handler,
        abnormal_exit_flag,
        session_id_shared,
        mut connection_failed_tx,
        child_pid,
        command_line,
        diagnostics_listener,
    } = params;

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
                async move |dispatch: Dispatch, _cx: ConnectionTo<Agent>| {
                    match dispatch {
                        Dispatch::Notification(message) => {
                            if SessionNotification::matches_method(&message.method) {
                                match SessionNotification::parse_message(&message.method, &message.params) {
                                    Ok(notification) => {
                                        handle_session_notification(notification, notifier.clone(), project_id.clone()).await;
                                        Ok(Handled::Yes)
                                    }
                                    Err(err) => {
                                        // 🔥 关键：未知消息类型只打 warn，不中断连接
                                        warn!(
                                            "[SACP] Failed to parse SessionNotification, ignoring: method={}, error={:?}, json={:?}",
                                            message.method, err, message.params
                                        );
                                        Ok(Handled::Yes)
                                    }
                                }
                            } else {
                                Ok(Handled::No {
                                    message: Dispatch::Notification(message),
                                    retry: false,
                                })
                            }
                        }
                        other => Ok(Handled::No {
                            message: other,
                            retry: false,
                        }),
                    }
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

            async move {
                // 1. 初始化连接（INIT_TIMEOUT_SECS 秒超时）
                info!(
                    "[SACP] Step 1/4: Initializing ACP connection, project_id={}",
                    project_id
                );
                let init_request = InitializeRequest::new(VERSION)
                    .client_info(agent_client_protocol::schema::Implementation::new(
                        "rcoder-agent-runner",
                        env!("CARGO_PKG_VERSION"),
                    ));
                debug!("[SACP] Sending InitializeRequest: {:?}", init_request);

                // 🔥 同时等待 InitializeRequest 和 cancel_token（由 lifecycle reaper 触发）
                // 不再使用 waitpid 轮询，避免与 lifecycle.rs 的 child_process.wait() 竞争
                let init_result = tokio::time::timeout(
                    std::time::Duration::from_secs(INIT_TIMEOUT_SECS),
                    async {
                        tokio::select! {
                            result = cx.send_request(init_request).block_task() => {
                                Ok(result)
                            }
                            _ = cancel_token.cancelled() => {
                                // lifecycle reaper 检测到子进程退出后会 cancel token
                                let cmd_info = format!("command=[{}]", command_line);
                                let is_abnormal = abnormal_exit_flag.load(Ordering::SeqCst);
                                if is_abnormal {
                                    Err(anyhow::anyhow!(
                                        "subprocess exited abnormally during init (pid={}) -- {}",
                                        child_pid, cmd_info
                                    ))
                                } else {
                                    Err(anyhow::anyhow!(
                                        "subprocess cancelled during init (pid={}) -- {}",
                                        child_pid, cmd_info
                                    ))
                                }
                            }
                        }
                    }
                ).await;

                let _init_response = match init_result {
                    Ok(Ok(result)) => {
                        // InitializeRequest 成功
                        result
                    }
                    Ok(Err(e)) => {
                        // send_request 返回错误 或 子进程退出
                        let err_msg = e.to_string();
                        error!("[SACP] Init phase error: {}, project_id={}", err_msg, project_id);
                        if let Some(tx) = connection_failed_tx.take() {
                            let _ = tx.send(err_msg.clone());
                        }
                        return Err(agent_client_protocol::Error::new(1003, err_msg));
                    }
                    Err(_elapsed) => {
                        error!(
                            "[SACP] ⏰ InitializeRequest timeout ({}s), project_id={}",
                            INIT_TIMEOUT_SECS, project_id
                        );
                        if let Some(tx) = connection_failed_tx.take() {
                            let _ = tx.send(format!(
                                "ACP InitializeRequest timeout ({}s), project_id={}",
                                INIT_TIMEOUT_SECS, project_id
                            ));
                        }
                        return Err(agent_client_protocol::Error::new(
                            1002,
                            format!(
                                "ACP InitializeRequest timeout ({}s), project_id={}",
                                INIT_TIMEOUT_SECS, project_id
                            ),
                        ));
                    }
                };
                info!(
                    "[SACP] Step 1/4: ACP connection initialized successfully, project_id={}",
                    project_id
                );

                // 2. 构建 meta（包含系统提示词和可能的 resume）
                let system_prompt_meta = start_config.build_meta();

                // 构建不含 resume 的 clean meta，用于 LoadSession 失败后回退到 NewSession
                // NewSession 应该是全新会话，不应携带旧的 resume session_id
                let new_session_meta = {
                    let mut meta = system_prompt_meta.clone();
                    if let Some(claude_code) = meta.get_mut("claudeCode").and_then(|v| v.as_object_mut())
                        && let Some(options) = claude_code.get_mut("options").and_then(|v| v.as_object_mut()) {
                            options.remove("resume");
                        }
                    meta
                };

                // 3. 创建或加载会话
                // 从配置获取超时值，默认 60 秒
                let timeout_secs = start_config
                    .acp_session_create_timeout_secs
                    .unwrap_or(60);
                info!(
                    "[SACP] Step 3/4: Creating/loading session, project_id={}, timeout={}s, has_resume={}",
                    project_id, timeout_secs, start_config.resume_session_id.is_some()
                );

                // 🔥 修复：使用 Result 累积错误，避免 ? 操作符提前返回
                // 无论成功失败，都确保能执行到 session_id_tx.send()
                let session_result: Result<SessionId, String> = if let Some(ref resume_id) =
                    start_config.resume_session_id
                {
                    // 有 resume_session_id，尝试加载历史会话
                    info!(
                        "[SACP] Attempting to load existing session: {}",
                        resume_id
                    );

                    let load_request = LoadSessionRequest::new(
                        resume_id.clone(),
                        project_path.clone(),
                    )
                    .mcp_servers(mcp_servers.clone())
                    .meta(system_prompt_meta.clone());

                    debug!("load_session_request: {:?}", load_request);

                    match tokio::time::timeout(
                        tokio::time::Duration::from_secs(timeout_secs),
                        cx.send_request(load_request).block_task(),
                    )
                    .await
                    {
                        Ok(Ok(_response)) => {
                            // LoadSession 成功，使用请求中的 session_id
                            info!(
                                "[SACP] Session loaded successfully: {}, resuming session",
                                resume_id
                            );
                            Ok(SessionId::from(resume_id.clone()))
                        }
                        Ok(Err(load_err)) => {
                            // LoadSession 返回错误，降级到 NewSessionRequest
                            warn!(
                                "[SACP] LoadSession failed, falling back to NewSession: {}",
                                load_err
                            );

                            let cancel_notification =
                                CancelNotification::new(SessionId::from(resume_id.clone()));
                            if let Err(e) = cx.send_notification(cancel_notification) {
                                debug!(
                                    "[SACP] Failed to send cancel notification for LoadSession: {}",
                                    e
                                );
                            }
                            // 等待一小段时间让 agent 有机会清理
                            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                            let new_request = NewSessionRequest::new(project_path.clone())
                                .mcp_servers(mcp_servers.clone())
                                .meta(new_session_meta.clone());

                            debug!("new_session_request: {:?}", new_request);

                            // 🔥 尝试 NewSession，不要用 ? 操作符
                            match tokio::time::timeout(
                                tokio::time::Duration::from_secs(timeout_secs),
                                cx.send_request(new_request).block_task(),
                            )
                            .await
                            {
                                Ok(Ok(response)) => Ok(response.session_id),
                                Ok(Err(new_err)) => Err(format!(
                                    "[SACP] LoadSession failed ({}), NewSession also failed ({})",
                                    load_err, new_err
                                )),
                                Err(_) => Err("[SACP] LoadSession failed (timeout), NewSession timeout".to_string()),
                            }
                        }
                        Err(_) => {
                            // LoadSession 超时，降级到 NewSessionRequest
                            warn!(
                                "[SACP] LoadSession timeout ({}s), falling back to NewSession",
                                timeout_secs
                            );

                            let cancel_notification =
                                CancelNotification::new(SessionId::from(resume_id.clone()));
                            if let Err(e) = cx.send_notification(cancel_notification) {
                                debug!(
                                    "[SACP] Failed to send cancel notification for LoadSession: {}",
                                    e
                                );
                            }
                            // 等待一小段时间让 agent 有机会清理
                            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

                            let new_request = NewSessionRequest::new(project_path.clone())
                                .mcp_servers(mcp_servers.clone())
                                .meta(new_session_meta.clone());

                            debug!("new_session_request: {:?}", new_request);

                            // 🔥 尝试 NewSession，不要用 ? 操作符
                            match tokio::time::timeout(
                                tokio::time::Duration::from_secs(timeout_secs),
                                cx.send_request(new_request).block_task(),
                            )
                            .await
                            {
                                Ok(Ok(response)) => Ok(response.session_id),
                                Ok(Err(new_err)) => Err(format!(
                                    "[SACP] LoadSession timeout, NewSession failed ({})",
                                    new_err
                                )),
                                Err(_) => {
                                    Err("[SACP] LoadSession timeout, NewSession timeout".to_string())
                                }
                            }
                        }
                    }
                } else {
                    // 没有 resume_session_id，创建新会话
                    info!("[SACP] Creating new ACP session (no resume_session_id)...");

                    let new_request = NewSessionRequest::new(project_path.clone())
                        .mcp_servers(mcp_servers.clone())
                        .meta(system_prompt_meta);

                    debug!("new_session_request: {:?}", new_request);

                    // 🔥 尝试 NewSession，不要用 ? 操作符
                    match tokio::time::timeout(
                        tokio::time::Duration::from_secs(timeout_secs),
                        cx.send_request(new_request).block_task(),
                    )
                    .await
                    {
                        Ok(Ok(response)) => Ok(response.session_id),
                        Ok(Err(e)) => Err(format!("[SACP] NewSession failed: {}", e)),
                        Err(_) => Err(format!(
                            "[SACP] NewSession timeout ({}s)",
                            timeout_secs
                        )),
                    }
                };

                // 🔥 关键修复：在闭包最后统一处理 session 创建结果
                // 确保无论成功失败都能执行到发送逻辑
                let session_id = match session_result {
                    Ok(sid) => sid,
                    Err(err_msg) => {
                        error!("[SACP] Session creation failed: {}", err_msg);
                        return Err(agent_client_protocol::Error::new(1000, err_msg));
                    }
                };

                info!(
                    "[SACP] ACP session ready, session_id={}",
                    session_id
                );

                // 🆕 当 agent_mode=ask 时，通过 ACP 协议设置 session mode
                // 只对已知 agent 发送对应的 mode，未知 agent 不设置
                if start_config.agent_mode == shared_types::AgentMode::Ask {
                    let cmd_lower = command_line.to_lowercase();
                    // 已知 agent 的 ask 模式映射
                    let target_mode = if cmd_lower.contains("claude-code") {
                        Some("default")  // claude-code-acp-ts: "default" 模式（危险操作需要审批）
                    } else {
                        None             // 其他 agent（含 nuwaxcode）: 使用默认行为
                    };

                    if let Some(mode) = target_mode {
                        let set_mode_request = SetSessionModeRequest::new(
                            session_id.clone(),
                            mode,
                        );
                        match cx.send_request(set_mode_request).block_task().await {
                            Ok(_) => {
                                info!(
                                    "[SACP] 🔒 Agent mode=ask, SetSessionModeRequest sent: session_id={}, mode={}",
                                    session_id, mode
                                );
                            }
                            Err(e) => {
                                warn!(
                                    "[SACP] Failed to set session mode to {}: session_id={}, error={}",
                                    mode, session_id, e
                                );
                            }
                        }
                    } else {
                        info!(
                            "[SACP] Agent mode=ask, no known mode mapping for agent: {}, using default behavior",
                            command_line
                        );
                    }
                }

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
                loop {
                    tokio::select! {
                        _ = cancel_token.cancelled() => {
                            // 🔥 检测取消原因，区分"正常取消"和"Agent 进程退出"
                            // 注意：如果在 prompt 处理中检测到取消，会在内层 loop 发送通知
                            // 这里只处理"没有正在处理的 prompt"时的情况
                            let is_abnormal = abnormal_exit_flag.load(Ordering::SeqCst);

                            if is_abnormal {
                                // Agent 进程异常退出，发送 SSE 错误通知
                                warn!(
                                    "[SACP] Agent process exited abnormally, sending SSE error notification and disconnecting: project_id={}, session_id={}",
                                    project_id_for_prompt, session_id
                                );
                                if let Err(e) = notifier_for_prompt
                                    .notify_prompt_error(
                                        &project_id_for_prompt,
                                        &session_id.to_string(),
                                        agent_client_protocol::Error::new(
                                            1001,
                                            error_codes::get_i18n_message_default("error.agent_process_abnormal_exit"),
                                        ),
                                        None, // request_id 可能已经不可用
                                    )
                                    .await
                                {
 error!("[SACP] send Agent error notification failed: {:?}", e);
                                } else {
 info!("[SACP] already sent Agent error notification: project_id={}", project_id_for_prompt);
                                }
                            } else {
                                // 🔥 修复：正常取消时也要发送 PromptEnd，确保状态回退 Idle
                                // 避免 Agent 一直卡在 Active 状态无法回收
                                if let Err(e) = notifier_for_prompt
                                    .notify_prompt_end(
                                        &project_id_for_prompt,
                                        &session_id.to_string(),
                                        agent_client_protocol::schema::StopReason::Cancelled,
                                        Some(error_codes::get_i18n_message_default("error.session_cancelled")),
                                        None,
                                    )
                                    .await
                                {
 error!("[SACP] send PromptEnd (Cancelled) notification failed: {:?}", e);
                                } else {
                                    info!(
                                        "[SACP] Sent PromptEnd (Cancelled) notification, state will revert to Idle: project_id={}, session_id={}",
                                        project_id_for_prompt, session_id
                                    );
                                }
                            }
                            break;
                        }
                        Some(cancel_request) = cancel_rx.recv() => {
                            let session_id_str = cancel_request.cancel_notification.session_id.0.to_string();
 info!("[SACP] received cancel request: session_id={}", session_id_str);
                            // 构建 SACP 版本的 CancelNotification 并发送到 Agent
                            let sacp_session_id = SessionId::new(Arc::from(session_id_str.as_str()));
                            let cancel_notification = CancelNotification::new(sacp_session_id);
                            if let Err(e) = cx.send_notification(cancel_notification) {
                                error!("[SACP] send cancel notification failed: {:?}", e);
                                // 通知调用方取消失败
                                let _ = cancel_request.result_tx.send(shared_types::CancelResult::Failed(
                                    format!("Failed to send cancel notification: {:?}", e)
                                ));
                            } else {
 info!("[SACP] cancel notification sent");
                                // 通知调用方取消成功
                                let _ = cancel_request.result_tx.send(shared_types::CancelResult::Success);
                                // 注意：故意不退出 outer loop（保持 Agent 进程存活以接收后续 prompt）
                                // 参见下方 prompt 分支的设计注释
                            }
                        }
                        Some(prompt_request) = prompt_rx.recv() => {
                            // 场景：用户快速发送 prompt A → cancel → prompt B
                            // - cancel 通知已发送给 Agent，但 outer loop 不退出
                            // - prompt B 到达时直接继续处理，保持 Agent 进程存活
                            debug!("[SACP] received Prompt request");

                            // 从 meta 中提取 request_id
                            let request_id = prompt_request
                                .meta
                                .as_ref()
                                .and_then(|meta| meta.get("request_id"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());

                            // 🎯 关键修复：通知状态管理器 Agent 开始处理 prompt
                            // 此时状态从 Pending -> Active，确保状态与 agent 实际执行同步
                            let session_id_str = session_id.to_string();
                            if let Err(e) = notifier_for_prompt
                                .notify_prompt_start(
                                    &project_id_for_prompt,
                                    &session_id_str,
                                    request_id.clone(),
                                )
                                .await
                            {
                                error!("[SACP] send PromptStart notification failed: {:?}", e);
                            } else {
                                info!(
                                    "[SACP] PromptStart notification sent: session_id={}, request_id={:?}",
                                    session_id_str, request_id
                                );
                            }

                            // 创建 Prompt 响应的 Future，使用 pin! 来固定它
                            let prompt_future = cx.send_request(prompt_request).block_task();
                            tokio::pin!(prompt_future);

                            // 取消后的超时保护：收到取消请求后最多等待 10 秒
                            let cancel_timeout = tokio::time::sleep(std::time::Duration::from_secs(3600)); // 初始设置一个很长的超时
                            tokio::pin!(cancel_timeout);
                            let mut is_cancelled = false;

                            // 在等待 Prompt 响应时也监听取消请求
                            let prompt_result = loop {
                                tokio::select! {
                                    biased;
                                    // 🔥 监听 cancel_token（Agent 进程退出时会触发）
                                    _ = cancel_token.cancelled() => {
                                        let is_abnormal = abnormal_exit_flag.load(Ordering::SeqCst);
                                        if is_abnormal {
                                            warn!(
                                                "[SACP] Detected Agent process abnormal exit during prompt processing: project_id={}, session_id={}",
                                                project_id_for_prompt, session_id
                                            );
                                            break Err(agent_client_protocol::Error::new(
                                                1001,
                                                error_codes::get_i18n_message_default("error.agent_process_abnormal_exit"),
                                            ));
                                        } else {
                                            // 正常取消（用户主动取消或 Agent 正常退出）
                                            info!(
                                                "[SACP] Received cancel signal during prompt processing: project_id={}, session_id={}",
                                                project_id_for_prompt, session_id
                                            );
                                            break Err(agent_client_protocol::Error::new(
                                                1002,
                                                error_codes::get_i18n_message_default("error.session_cancelled"),
                                            ));
                                        }
                                    }
                                    // 取消后的超时保护（只有 is_cancelled 为 true 时才有意义）
                                    _ = &mut cancel_timeout, if is_cancelled => {
                                        // 取消后超时，强制返回错误
                                        warn!("[SACP] cancel message Prompt response timeout (10s), force exit");
                                        break Err(agent_client_protocol::Error::new(
                                            1001,
                                            error_codes::get_i18n_message_default("error.cancel_response_timeout"),
                                        ));
                                    }
                                    // 检查取消请求（无论是否已取消都要接收，避免调用方超时）
                                    Some(cancel_request) = cancel_rx.recv() => {
                                        if is_cancelled {
                                            // 🎯 已经在取消中，直接返回成功（通知已发送）
                                            info!("[SACP] already sent cancel request, notification succeeded");
                                            let _ = cancel_request.result_tx.send(shared_types::CancelResult::Success);
                                        } else {
                                            let session_id_str = cancel_request.cancel_notification.session_id.0.to_string();
                                            info!("[SACP] received Prompt cancel request: session_id={}", session_id_str);
                                            // 发送取消通知给 Agent
                                            let sacp_session_id = SessionId::new(Arc::from(session_id_str.as_str()));
                                            let cancel_notification = CancelNotification::new(sacp_session_id);
                                            if let Err(e) = cx.send_notification(cancel_notification) {
                                                error!("[SACP] send cancel notification failed: {:?}", e);
                                                // 发送失败立即返回错误
                                                let _ = cancel_request.result_tx.send(shared_types::CancelResult::Failed(
                                                    format!("Failed to send cancel notification: {:?}", e)
                                                ));
                                            } else {
                                                info!("[SACP] cancel notification sent");
                                                // 🎯 立即返回成功，不阻塞调用方
                                                let _ = cancel_request.result_tx.send(shared_types::CancelResult::Success);
                                                is_cancelled = true;
                                                // 设置超时保护：取消后最多等待 10 秒让 prompt 完成
                                                cancel_timeout.as_mut().reset(tokio::time::Instant::now() + std::time::Duration::from_secs(10));
                                            }
                                        }
                                        // 继续等待 Prompt 响应（Agent 应该会因为取消而提前返回）
                                    }
                                    result = &mut prompt_future => {
                                        // Prompt 响应完成
                                        break result;
                                    }
                                }
                            };

                            // 处理 Prompt 响应结果
                            match prompt_result {
                                Ok(response) => {
 debug!("[SACP] Prompt response: stop_reason={:?}", response.stop_reason);
                                    // 发送 PromptEnd 通知
                                    if let Err(e) = notifier_for_prompt
                                        .notify_prompt_end(
                                            &project_id_for_prompt,
                                            &session_id.to_string(),
                                            response.stop_reason,
                                            None,
                                            request_id.clone(),
                                        )
                                        .await
                                    {
                                        error!("[SACP] send PromptEnd notification failed: {:?}", e);
                                    } else {
                                        info!(
                                            "[SACP] PromptEnd notification sent: session_id={}, request_id={:?}",
                                            session_id, request_id
                                        );
                                    }
                                }
                                Err(e) => {
                                    // 🎯 区分"取消超时"和"真正的错误"
                                    if is_cancelled {
                                        // 取消超时：发送 PromptEnd (Cancelled) 而非 PromptError
                                        info!("[SACP] cancel timeout, send PromptEnd (Cancelled): session_id={}", session_id);
                                        if let Err(notify_err) = notifier_for_prompt
                                            .notify_prompt_end(
                                                &project_id_for_prompt,
                                                &session_id.to_string(),
                                                agent_client_protocol::schema::StopReason::Cancelled,
                                                Some(error_codes::get_i18n_message_default("error.session_cancelled_timeout")),
                                                request_id.clone(),
                                            )
                                            .await
                                        {
                                            error!("[SACP] send PromptEnd (Cancelled) notification failed: {:?}", notify_err);
                                        }
                                    } else {
                                        // 真正的错误：发送 PromptError
 error!("[SACP] Prompt request failed: {:?}", e);
                                        if let Err(notify_err) = notifier_for_prompt
                                            .notify_prompt_error(
                                                &project_id_for_prompt,
                                                &session_id.to_string(),
                                                e,
                                                request_id.clone(),
                                            )
                                            .await
                                        {
 error!("[SACP] send PromptError notification failed: {:?}", notify_err);
                                        }
                                    }

                                    // 🔥 关键：如果 cancel_token 已取消，直接退出外层 loop
                                    // 避免回到外层 loop 时再次触发 cancel_token.cancelled() 导致重复发送通知
                                    if cancel_token.is_cancelled() {
 info!("[SACP] Prompt completed but cancel_token already cancelled, exiting");
                                        break;
                                    }
                                }
                            }

                            // 🎯 关键设计：cancel 后不退出 outer loop，保持 Agent 子进程存活
                            //
                            // 为什么不能 break outer loop：
                            // - outer loop break → spawned task 退出 → lifecycle_guard drop
                            // - LifecycleGuard::drop() → SIGKILL → Agent 子进程被杀
                            // - 子进程被杀 → 内存中的对话上下文丢失
                            // - 下次请求 get_or_create_session → is_channel_closed()=true → 创建新 session → 上下文断裂
                            //
                            // 正确行为：
                            // - inner loop 处理了 cancel → is_cancelled=true → inner loop 退出
                            // - notify_prompt_end(Cancelled) → 状态恢复 Idle
                            // - outer loop 继续等待 prompt_rx.recv() → 收到新 prompt → 复用同一 Agent 进程
                            // - 上下文连续：同一子进程、同一 SACP 连接、同一对话历史
                            //
                            // is_cancelled 仅是 inner loop 的局部标志，不退出 outer loop。
                            info!(
                                "[SACP] Prompt cancelled, session ready for next prompt: project_id={}, session_id={}",
                                project_id_for_prompt, session_id
                            );
                        }
                        else => {
                            // 所有通道已关闭
 info!("[SACP] channels already closed, exiting");
                            break;
                        }
                    }
                }

                Ok(())
            }
        })
        .await?;

    Ok(())
}

/// 处理 SessionNotification 回调
async fn handle_session_notification<N: SessionNotifier>(
    notification: SessionNotification,
    notifier: Arc<N>,
    project_id: String,
) {
    let session_id = notification.session_id.to_string();

    debug!(
        "[SACP] SessionNotification: project_id={}, session_id={}, update={:?}",
        project_id, session_id, notification.update
    );

    // 提取 request_id（如果有）
    let request_id = notification
        .meta
        .as_ref()
        .and_then(|meta| meta.get("request_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // 通过 notifier 推送会话更新
    // SessionUpdate 通过 agent_client_protocol::schema 导入
    if let Err(e) = notifier
        .notify_session_update(&project_id, &session_id, notification.update, request_id)
        .await
    {
        error!(
            "[SACP] Push session update failed: project_id={}, session_id={}, error={:?}",
            project_id, session_id, e
        );
    }
}
