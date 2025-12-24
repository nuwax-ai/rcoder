//! 通道工具模块
//!
//! 提供代理通信所需的通道处理工具函数

use crate::acp::{CancelNotificationRequestWrapper, CancelResult};
use crate::traits::{SessionNotifier, SessionRegistry};
use agent_client_protocol::{
    Agent, ClientSideConnection, McpServer, NewSessionRequest, PromptRequest, SessionId,
};
use chrono::Utc;
use shared_types::{AgentLifecycle, AgentStatus, ModelProviderConfig, ProjectAndAgentInfo};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Prompt 处理器配置
///
/// 使用泛型 `R: SessionRegistry` 替代直接依赖 DashMap，支持依赖注入
pub struct PromptHandlerConfig<N: SessionNotifier, R: SessionRegistry> {
    /// 是否是 resume 会话
    pub is_resume_session: bool,
    /// 项目路径（用于降级时创建新会话）
    pub project_path: PathBuf,
    /// MCP 服务器配置（用于降级时创建新会话）
    pub mcp_servers: Vec<McpServer>,
    /// 会话注册表（用于降级时更新）
    pub registry: Arc<R>,
    /// Cancel 通道（用于降级时创建新会话条目）
    pub cancel_tx: mpsc::UnboundedSender<CancelNotificationRequestWrapper>,
    /// 生命周期句柄（用于降级时创建新会话条目）
    pub lifecycle_handle: Option<Arc<dyn AgentLifecycle>>,
    /// 模型配置（用于降级时创建新会话条目）
    pub model_provider: Option<ModelProviderConfig>,
    /// 通知器
    pub notifier: Arc<N>,
}

/// 为 Agent 启动取消处理器
///
/// 处理取消请求并通过 oneshot channel 返回结果给调用方
pub fn spawn_cancel_handler_for_agent(
    client_conn: Arc<ClientSideConnection>,
    mut cancel_rx: mpsc::UnboundedReceiver<CancelNotificationRequestWrapper>,
    project_id: &str,
) {
    let project_id = project_id.to_string();
    tokio::task::spawn_local(async move {
        while let Some(cancel_request_wrapper) = cancel_rx.recv().await {
            info!("项目[{}]收到取消请求", project_id);

            // 直接从包装器中提取 CancelNotification 和结果通道
            let cancel_notification = cancel_request_wrapper.cancel_notification;
            let result_tx = cancel_request_wrapper.result_tx;

            // 添加超时保护，防止 Agent cancel 调用阻塞
            let cancel_result = tokio::time::timeout(
                tokio::time::Duration::from_secs(10),
                client_conn.cancel(cancel_notification),
            )
            .await;

            // 根据结果发送响应
            let result = match cancel_result {
                Ok(Ok(_)) => {
                    info!("项目[{}]Agent取消成功", project_id);
                    CancelResult::Success
                }
                Ok(Err(e)) => {
                    let error_msg = format!("{:?}", e);
                    error!("项目[{}]发送Cancel失败: {}", project_id, error_msg);
                    CancelResult::Failed(error_msg)
                }
                Err(_timeout_err) => {
                    warn!("项目[{}]Agent取消超时", project_id);
                    CancelResult::Timeout
                }
            };

            // 通过 oneshot channel 返回结果
            if let Err(e) = result_tx.send(result) {
                error!(
                    "项目[{}]发送取消结果失败（接收方已关闭）: {:?}",
                    project_id, e
                );
            }
        }

        info!("项目[{}]Cancel处理任务结束", project_id);
    });
}

/// 为 Agent 启动提示处理器
///
/// # 参数
/// - `client_conn`: ACP 客户端连接
/// - `prompt_rx`: Prompt 消息接收通道
/// - `session_id`: 当前会话 ID
/// - `project_id`: 项目 ID
/// - `config`: Prompt 处理器配置（包含降级所需的所有信息）
///
/// # 降级机制
/// 当 resume 会话的首次 Prompt 失败时，会在内部完成降级：
/// 1. 创建新会话（不带 resume）
/// 2. 更新 registry 中的会话信息
/// 3. 重试 Prompt
/// 4. 继续处理后续 Prompt
pub fn spawn_prompt_handler_for_agent<N: SessionNotifier + 'static, R: SessionRegistry + 'static>(
    client_conn: Arc<ClientSideConnection>,
    mut prompt_rx: mpsc::UnboundedReceiver<PromptRequest>,
    session_id: SessionId,
    project_id: &str,
    config: PromptHandlerConfig<N, R>,
) where
    R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
{
    let project_id = project_id.to_string();
    let mut current_session_id = session_id;
    let mut session_id_str = current_session_id.0.clone();

    // 提取配置
    let is_resume_session = config.is_resume_session;
    let project_path = config.project_path;
    let mcp_servers = config.mcp_servers;
    let registry = config.registry;
    let cancel_tx = config.cancel_tx;
    let lifecycle_handle = config.lifecycle_handle;
    let model_provider = config.model_provider;
    let notifier = config.notifier;

    tokio::task::spawn_local(async move {
        info!(
            "🚀 项目[{}]Prompt处理任务已启动，开始监听消息... (is_resume={})",
            project_id, is_resume_session
        );

        // 追踪是否是第一个 Prompt（用于 resume 降级检测）
        let mut is_first_prompt = true;
        // 追踪是否已经降级过（每个会话只降级一次）
        let mut has_fallback = false;

        while let Some(mut req) = prompt_rx.recv().await {
            info!("📨 项目[{}]从prompt_rx接收到Prompt消息", project_id);

            // 如果收到的 session_id 与当前不一致，强制覆盖
            if req.session_id.0 != current_session_id.0 {
                warn!(
                    "项目[{}]收到Prompt的session_id({})与当前agent会话({})不一致，强制覆盖为当前会话",
                    project_id,
                    req.session_id.0,
                    current_session_id.0
                );
                req.session_id = current_session_id.clone();
            }

            info!(
                "项目[{}]收到Prompt消息, session_id={}",
                project_id, req.session_id.0
            );

            // 从 PromptRequest.meta 中提取 request_id
            let request_id = if let Some(ref meta) = req.meta {
                let req_id = meta
                    .get("request_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                debug!(
                    "🔍 项目[{}] 从 PromptRequest.meta 提取 request_id={:?}",
                    project_id, req_id
                );
                req_id
            } else {
                debug!("⚠️ 项目[{}] PromptRequest.meta 为空", project_id);
                None
            };

            // 发送 SessionPromptStart 通知
            if let Err(e) = notifier
                .notify_prompt_start(&project_id, &session_id_str, request_id.clone())
                .await
            {
                error!("项目[{}]发送SessionPromptStart失败: {:?}", project_id, e);
            }

            // 调用 Agent 处理 prompt
            match client_conn.prompt(req.clone()).await {
                Ok(resp) => {
                    info!(
                        "项目[{}]Prompt发送成功, stop_reason={:?}",
                        project_id, resp.stop_reason
                    );

                    // 发送 SessionPromptEnd 通知
                    if let Err(e) = notifier
                        .notify_prompt_end(
                            &project_id,
                            &session_id_str,
                            resp.stop_reason,
                            None,
                            request_id.clone(),
                        )
                        .await
                    {
                        error!("项目[{}]发送SessionPromptEnd失败: {:?}", project_id, e);
                    }

                    // 第一个 Prompt 成功，说明 resume 有效
                    is_first_prompt = false;
                }
                Err(e) => {
                    let error_message = e.message.clone();
                    error!("项目[{}]发送Prompt失败: {:?}", project_id, error_message);

                    // Resume 降级逻辑：
                    // 如果是 resume 会话的第一个 Prompt 失败，直接在内部完成降级
                    let should_fallback = is_first_prompt && is_resume_session && !has_fallback;

                    if should_fallback {
                        warn!(
                            "⚠️ 项目[{}] Resume 会话首次 Prompt 失败，开始内部降级: {}",
                            project_id, error_message
                        );

                        // 在内部完成降级
                        match handle_fallback_internal(
                            &client_conn,
                            &project_id,
                            &project_path,
                            &mcp_servers,
                            &registry,
                            &cancel_tx,
                            lifecycle_handle.clone(),
                            model_provider.clone(),
                            &notifier,
                            req,
                            request_id.clone(),
                        )
                        .await
                        {
                            Ok(new_session_id) => {
                                info!(
                                    "✅ 项目[{}] 降级成功，新会话 ID: {}",
                                    project_id, new_session_id.0
                                );
                                // 更新当前 session_id
                                current_session_id = new_session_id;
                                session_id_str = current_session_id.0.clone();
                                has_fallback = true;
                                is_first_prompt = false;
                                // 继续处理后续 Prompt，不退出
                                continue;
                            }
                            Err(fallback_err) => {
                                error!(
                                    "❌ 项目[{}] 降级失败: {:?}",
                                    project_id, fallback_err
                                );
                                has_fallback = true;
                                // 降级失败，继续走正常错误处理流程
                            }
                        }
                    }

                    // 正常错误处理流程
                    // 发送 SessionPromptError 通知
                    if let Err(notify_err) = notifier
                        .notify_prompt_error(
                            &project_id,
                            &session_id_str,
                            e,
                            request_id.clone(),
                        )
                        .await
                    {
                        error!(
                            "项目[{}]发送SessionPromptError失败: {:?}",
                            project_id, notify_err
                        );
                    }

                    // 发送 SessionPromptEnd 通知，标记会话结束
                    if let Err(notify_err) = notifier
                        .notify_prompt_end(
                            &project_id,
                            &session_id_str,
                            agent_client_protocol::StopReason::Cancelled,
                            Some(error_message),
                            request_id.clone(),
                        )
                        .await
                    {
                        error!(
                            "项目[{}]发送SessionPromptEnd失败: {:?}",
                            project_id, notify_err
                        );
                    }

                    // 第一个 Prompt 处理完毕（无论成功失败）
                    is_first_prompt = false;
                }
            }
        }

        info!("项目[{}]Prompt处理任务结束", project_id);
    });
}

/// 内部降级处理
///
/// 在 Prompt 处理器内部完成降级：
/// 1. 创建新会话（不带 resume）
/// 2. 更新 registry 中的会话信息
/// 3. 重试 Prompt
async fn handle_fallback_internal<N: SessionNotifier, R: SessionRegistry>(
    client_conn: &Arc<ClientSideConnection>,
    project_id: &str,
    project_path: &PathBuf,
    mcp_servers: &[McpServer],
    registry: &Arc<R>,
    cancel_tx: &mpsc::UnboundedSender<CancelNotificationRequestWrapper>,
    lifecycle_handle: Option<Arc<dyn AgentLifecycle>>,
    model_provider: Option<ModelProviderConfig>,
    notifier: &Arc<N>,
    failed_prompt: PromptRequest,
    request_id: Option<String>,
) -> anyhow::Result<SessionId>
where
    R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
{
    info!("⚠️ 项目[{}] 开始 Resume 降级重建", project_id);

    // 1. 创建新会话（不带 resume meta）
    let new_session_request = NewSessionRequest::new(project_path.clone())
        .mcp_servers(mcp_servers.to_vec());

    let resp = client_conn
        .new_session(new_session_request)
        .await
        .map_err(|e| anyhow::anyhow!("降级创建新会话失败: {:?}", e))?;

    let new_session_id = resp.session_id;
    info!(
        "✅ 项目[{}] 降级新会话创建成功: {}",
        project_id, new_session_id.0
    );

    // 2. 创建新的 prompt channel（因为原来的 rx 在当前任务中）
    let (new_prompt_tx, _new_prompt_rx) = mpsc::unbounded_channel::<PromptRequest>();

    // 3. 创建 ProjectAndAgentInfo 并更新 registry
    let now = Utc::now();
    let new_agent_info = ProjectAndAgentInfo {
        project_id: project_id.to_string(),
        session_id: new_session_id.clone(),
        prompt_tx: new_prompt_tx,
        cancel_tx: cancel_tx.clone(),
        model_provider,
        request_id: None,
        status: AgentStatus::Idle,
        last_activity: now,
        created_at: now,
        stop_handle: lifecycle_handle,
    };

    // 通过 registry 更新会话信息
    let session_id_str_for_registry = new_session_id.0.to_string();
    registry.insert(project_id, &session_id_str_for_registry, new_agent_info.into());
    info!(
        "✅ 项目[{}] 已更新 registry，新 session_id: {}",
        project_id, new_session_id.0
    );

    // 4. 重试 Prompt
    let mut retry_prompt = failed_prompt;
    retry_prompt.session_id = new_session_id.clone();

    let session_id_str = new_session_id.0.clone();

    // 发送 SessionPromptStart 通知（降级重试）
    if let Err(e) = notifier
        .notify_prompt_start(project_id, &session_id_str, request_id.clone())
        .await
    {
        error!(
            "项目[{}] 降级重试发送 SessionPromptStart 失败: {:?}",
            project_id, e
        );
    }

    // 直接调用 prompt 发送
    match client_conn.prompt(retry_prompt).await {
        Ok(resp) => {
            info!(
                "✅ 项目[{}] 降级重试 Prompt 发送成功, stop_reason={:?}",
                project_id, resp.stop_reason
            );

            // 发送 SessionPromptEnd 通知
            if let Err(e) = notifier
                .notify_prompt_end(
                    project_id,
                    &session_id_str,
                    resp.stop_reason,
                    None,
                    request_id,
                )
                .await
            {
                error!(
                    "项目[{}] 降级重试发送 SessionPromptEnd 失败: {:?}",
                    project_id, e
                );
            }
        }
        Err(e) => {
            let error_message = e.message.clone();
            error!(
                "❌ 项目[{}] 降级重试 Prompt 发送失败: {:?}",
                project_id, error_message
            );

            // 发送错误通知
            if let Err(notify_err) = notifier
                .notify_prompt_error(project_id, &session_id_str, e, request_id.clone())
                .await
            {
                error!(
                    "项目[{}] 降级重试发送 SessionPromptError 失败: {:?}",
                    project_id, notify_err
                );
            }

            // 发送 SessionPromptEnd 通知
            if let Err(notify_err) = notifier
                .notify_prompt_end(
                    project_id,
                    &session_id_str,
                    agent_client_protocol::StopReason::Cancelled,
                    Some(error_message.clone()),
                    request_id,
                )
                .await
            {
                error!(
                    "项目[{}] 降级重试发送 SessionPromptEnd 失败: {:?}",
                    project_id, notify_err
                );
            }

            // 降级重试也失败，返回错误
            return Err(anyhow::anyhow!(
                "降级重试 Prompt 失败: {}",
                error_message
            ));
        }
    }

    info!("✅ 项目[{}] 降级重建完成", project_id);
    Ok(new_session_id)
}
