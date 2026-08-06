//! SACP 连接建立与会话准备
//!
//! 从 `run_sacp_connection` 抽出的三个阶段：
//! 1. InitializeRequest（带超时与 cancel_token 竞速）
//! 2. 会话 meta 构建 + LoadSession/NewSession 创建或加载
//! 3. agent_mode=ask 时的 SetSessionModeRequest

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use agent_client_protocol::schema::v1::{
    CancelNotification, InitializeRequest, LoadSessionRequest, McpServer, NewSessionRequest,
    SessionId, SetSessionModeRequest,
};
use agent_client_protocol::{Agent, ConnectionTo};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::super::types::VERSION;
use crate::traits::AgentStartConfig;

/// InitializeRequest 超时时间（秒）
pub(super) const INIT_TIMEOUT_SECS: u64 = 50;

/// Step 1: 初始化 ACP 连接（INIT_TIMEOUT_SECS 秒超时，同时与 cancel_token 竞速）
pub(super) async fn initialize_connection(
    cx: &ConnectionTo<Agent>,
    project_id: &str,
    cancel_token: &CancellationToken,
    command_line: &str,
    abnormal_exit_flag: &AtomicBool,
    child_pid: u32,
    connection_failed_tx: &mut Option<tokio::sync::oneshot::Sender<String>>,
) -> Result<(), agent_client_protocol::Error> {
    info!(
        "[SACP] Step 1/4: Initializing ACP connection, project_id={}",
        project_id
    );
    let init_request = InitializeRequest::new(VERSION).client_info(
        agent_client_protocol::schema::v1::Implementation::new(
            "rcoder-agent-runner",
            env!("CARGO_PKG_VERSION"),
        ),
    );
    debug!("[SACP] Sending InitializeRequest: {:?}", init_request);

    // 🔥 同时等待 InitializeRequest 和 cancel_token（由 lifecycle reaper 触发）
    // 不再使用 waitpid 轮询，避免与 lifecycle.rs 的 child_process.wait() 竞争
    let init_result =
        tokio::time::timeout(std::time::Duration::from_secs(INIT_TIMEOUT_SECS), async {
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
        })
        .await;

    let _init_response = match init_result {
        Ok(Ok(result)) => {
            // InitializeRequest 成功
            result
        }
        Ok(Err(e)) => {
            // send_request 返回错误 或 子进程退出
            let err_msg = e.to_string();
            error!(
                "[SACP] Init phase error: {}, project_id={}",
                err_msg, project_id
            );
            if let Some(tx) = connection_failed_tx.take()
                && let Err(send_err) = tx.send(err_msg.clone())
            {
                warn!(
                    "[SACP] connection_failed_tx send failed (receiver dropped), error was: {}",
                    send_err
                );
            }
            return Err(agent_client_protocol::Error::new(1003, err_msg));
        }
        Err(_elapsed) => {
            error!(
                "[SACP] ⏰ InitializeRequest timeout ({}s), project_id={}",
                INIT_TIMEOUT_SECS, project_id
            );
            if let Some(tx) = connection_failed_tx.take()
                && let Err(send_err) = tx.send(format!(
                    "ACP InitializeRequest timeout ({}s), project_id={}",
                    INIT_TIMEOUT_SECS, project_id
                ))
            {
                warn!(
                    "[SACP] connection_failed_tx send failed (receiver dropped), error was: {}",
                    send_err
                );
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
    Ok(())
}

/// Step 2/3: 构建会话 meta 并创建或加载会话
///
/// - 有 resume_session_id 时优先 LoadSession，失败/超时降级到 NewSession
/// - 无 resume_session_id 时直接 NewSession
/// - 不使用 ? 提前返回，确保错误以字符串累积后统一映射为协议错误
pub(super) async fn create_or_load_session(
    cx: &ConnectionTo<Agent>,
    project_id: &str,
    project_path: PathBuf,
    mcp_servers: Vec<McpServer>,
    start_config: &AgentStartConfig,
) -> Result<SessionId, agent_client_protocol::Error> {
    // 2. 构建 meta（包含系统提示词和可能的 resume）
    let system_prompt_meta = start_config.build_meta();

    // 构建不含 resume 的 clean meta，用于 LoadSession 失败后回退到 NewSession
    // NewSession 应该是全新会话，不应携带旧的 resume session_id
    let new_session_meta = {
        let mut meta = system_prompt_meta.clone();
        if let Some(claude_code) = meta.get_mut("claudeCode").and_then(|v| v.as_object_mut())
            && let Some(options) = claude_code
                .get_mut("options")
                .and_then(|v| v.as_object_mut())
        {
            options.remove("resume");
        }
        meta
    };

    // 3. 创建或加载会话
    // 从配置获取超时值，默认 60 秒
    let timeout_secs = start_config.acp_session_create_timeout_secs.unwrap_or(60);
    info!(
        "[SACP] Step 3/4: Creating/loading session, project_id={}, timeout={}s, has_resume={}",
        project_id,
        timeout_secs,
        start_config.resume_session_id.is_some()
    );

    // 🔥 修复：使用 Result 累积错误，避免 ? 操作符提前返回
    // 无论成功失败，都确保能执行到 session_id_tx.send()
    let session_result: Result<SessionId, String> = if let Some(ref resume_id) =
        start_config.resume_session_id
    {
        // 有 resume_session_id，尝试加载历史会话
        info!("[SACP] Attempting to load existing session: {}", resume_id);

        let load_request = LoadSessionRequest::new(resume_id.clone(), project_path.clone())
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
                    Err(_) => {
                        Err("[SACP] LoadSession failed (timeout), NewSession timeout".to_string())
                    }
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
                    Err(_) => Err("[SACP] LoadSession timeout, NewSession timeout".to_string()),
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
            Err(_) => Err(format!("[SACP] NewSession timeout ({}s)", timeout_secs)),
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

    Ok(session_id)
}

/// 🆕 当 agent_mode=ask 时，通过 ACP 协议设置 session mode
/// 只对已知 agent 发送对应的 mode，未知 agent 不设置
pub(super) async fn apply_ask_session_mode(
    cx: &ConnectionTo<Agent>,
    start_config: &AgentStartConfig,
    command_line: &str,
    session_id: &SessionId,
) {
    if start_config.agent_mode == shared_types::AgentMode::Ask {
        let cmd_lower = command_line.to_lowercase();
        // 已知 agent 的 ask 模式映射
        let target_mode = if cmd_lower.contains("claude-code") {
            Some("default") // claude-code-acp-ts: "default" 模式（危险操作需要审批）
        } else {
            None // 其他 agent（含 nuwaxcode）: 使用默认行为
        };

        if let Some(mode) = target_mode {
            let set_mode_request = SetSessionModeRequest::new(session_id.clone(), mode);
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
}
