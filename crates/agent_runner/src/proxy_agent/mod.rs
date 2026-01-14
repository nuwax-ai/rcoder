mod acp_agent;
pub mod cleanup_task;

use crate::CancelNotificationRequestWrapper;
use crate::{
    model::{AgentSessionUpdate, SessionNotify},
    service::{AGENT_REGISTRY, push_session_update},
};
// 导出 agent_worker 相关类型和函数
pub use acp_agent::{LocalSetAgentRequest, agent_worker, agent_worker_with_heartbeat};
use agent_abstraction::launcher::AgentStopHandleArc;
use agent_client_protocol::{Client, PermissionOptionKind, PromptRequest, SessionId};
use dashmap::DashMap;
use std::sync::LazyLock;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

/// 会话级别的 request_id 上下文映射（project_id -> request_id）
/// 用于在 session_notification 回调中获取当前请求的 request_id
/// 避免使用 PROJECT_AND_AGENT_INFO_MAP 导致的锁竞争问题
/// 注意：使用 project_id 而非 session_id，确保同一项目的多次请求能自动覆盖为最新值
pub static SESSION_REQUEST_CONTEXT: LazyLock<DashMap<String, String>> = LazyLock::new(DashMap::new);

/// ACP协议的连接信息
pub struct AcpConnectionInfo {
    /// 会话ID
    pub session_id: SessionId,
    /// 用于发送 Prompt 的通道
    pub prompt_tx: mpsc::UnboundedSender<PromptRequest>,
    /// 用于发送取消通知的通道（使用新类型）
    pub cancel_tx: mpsc::UnboundedSender<CancelNotificationRequestWrapper>,
    /// Agent停止句柄（将被包装为守卫并放入 ProjectAndAgentInfo）
    pub stop_handle: Option<AgentStopHandleArc>,
}

/// ACP 客户端实现
#[derive(Clone, Default)]
pub struct AcpAgentClient;

#[async_trait::async_trait(?Send)]
impl Client for AcpAgentClient {
    async fn request_permission(
        &self,
        args: agent_client_protocol::RequestPermissionRequest,
    ) -> Result<agent_client_protocol::RequestPermissionResponse, agent_client_protocol::Error>
    {
        debug!("请求权限: {:?}", args);
        // 自动允许：优先选择 AllowAlways，其次 AllowOnce；若都无，选第一个选项
        let selected = args
            .options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::AllowAlways)
            .or_else(|| {
                args.options
                    .iter()
                    .find(|o| o.kind == PermissionOptionKind::AllowOnce)
            })
            .or_else(|| args.options.first());
        if let Some(option) = selected {
            return Ok(agent_client_protocol::RequestPermissionResponse::new(
                agent_client_protocol::RequestPermissionOutcome::Selected(
                    agent_client_protocol::SelectedPermissionOutcome::new(option.option_id.clone()),
                ),
            ));
        }
        // 无可选项则取消
        Ok(agent_client_protocol::RequestPermissionResponse::new(
            agent_client_protocol::RequestPermissionOutcome::Cancelled,
        ))
    }

    async fn write_text_file(
        &self,
        args: agent_client_protocol::WriteTextFileRequest,
    ) -> Result<agent_client_protocol::WriteTextFileResponse, agent_client_protocol::Error> {
        debug!("写入文件: {:?}", args);
        if let Some(parent) = std::path::Path::new(&args.path).parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                error!("创建目录失败: {}", e);
                agent_client_protocol::Error::internal_error()
            })?;
        }
        let mut file = tokio::fs::File::create(&args.path).await.map_err(|e| {
            error!("创建文件失败: {}", e);
            agent_client_protocol::Error::internal_error()
        })?;
        file.write_all(args.content.as_bytes()).await.map_err(|e| {
            error!("写入文件失败: {}", e);
            agent_client_protocol::Error::internal_error()
        })?;
        Ok(agent_client_protocol::WriteTextFileResponse::new())
    }

    async fn read_text_file(
        &self,
        args: agent_client_protocol::ReadTextFileRequest,
    ) -> Result<agent_client_protocol::ReadTextFileResponse, agent_client_protocol::Error> {
        debug!("读取文件: {:?}", args);
        let content = tokio::fs::read_to_string(&args.path).await.map_err(|e| {
            error!("读取文件失败: {}", e);
            agent_client_protocol::Error::internal_error()
        })?;
        Ok(agent_client_protocol::ReadTextFileResponse::new(content))
    }

    async fn create_terminal(
        &self,
        _request: agent_client_protocol::CreateTerminalRequest,
    ) -> Result<agent_client_protocol::CreateTerminalResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn terminal_output(
        &self,
        _request: agent_client_protocol::TerminalOutputRequest,
    ) -> Result<agent_client_protocol::TerminalOutputResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn release_terminal(
        &self,
        _request: agent_client_protocol::ReleaseTerminalRequest,
    ) -> Result<agent_client_protocol::ReleaseTerminalResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn wait_for_terminal_exit(
        &self,
        _request: agent_client_protocol::WaitForTerminalExitRequest,
    ) -> Result<agent_client_protocol::WaitForTerminalExitResponse, agent_client_protocol::Error>
    {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn kill_terminal_command(
        &self,
        _request: agent_client_protocol::KillTerminalCommandRequest,
    ) -> Result<agent_client_protocol::KillTerminalCommandResponse, agent_client_protocol::Error>
    {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn session_notification(
        &self,
        args: agent_client_protocol::SessionNotification,
    ) -> Result<(), agent_client_protocol::Error> {
        let session_id_str = args.session_id.to_string();

        // 先尝试从 SessionNotification.meta 中获取 request_id
        let request_id_from_notification = args
            .meta
            .as_ref()
            .and_then(|meta| meta.get("request_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if let Some(ref req_id) = request_id_from_notification {
            debug!(
                "✅ [session_notification] session_id={} 从 SessionNotification.meta 获取 request_id={}",
                session_id_str, req_id
            );
        }

        // 如果 SessionNotification.meta 中没有 request_id，则通过 session_id 查找 project_id，再从 SESSION_REQUEST_CONTEXT 获取
        let request_id = request_id_from_notification.or_else(|| {
            // 使用统一 Registry 的 O(1) 反向查询（替代旧的 O(n) 遍历）
            AGENT_REGISTRY
                .get_project_by_session(&session_id_str)
                .and_then(|project_id| {
                    // 使用 project_id 从 SESSION_REQUEST_CONTEXT 获取 request_id
                    SESSION_REQUEST_CONTEXT.get(&project_id).map(|entry| {
                        let req_id = entry.value().clone();
                        debug!(
                            "🔍 [session_notification] session_id={} -> project_id={} 从 SESSION_REQUEST_CONTEXT 获取 request_id={}",
                            session_id_str, project_id, req_id
                        );
                        req_id
                    })
                })
        });

        if request_id.is_none() {
            debug!(
                "⚠️ [session_notification] session_id={} 未找到 request_id（SessionNotification.meta 和 SESSION_REQUEST_CONTEXT 中都没有）",
                session_id_str
            );
        }

        let agent_update = AgentSessionUpdate {
            session_id: session_id_str.clone(),
            session_update: args.update.clone(),
            request_id,
        };
        let notify = SessionNotify::AgentSessionUpdate(agent_update);
        if let Err(e) = push_session_update(&session_id_str, notify).await {
            error!(
                "❌ Failed to cache SessionUpdate for session {}: {}",
                session_id_str, e
            );
        }

        // 记录日志（保持原有的详细日志）
        match &args.update {
            agent_client_protocol::SessionUpdate::AgentMessageChunk(content_chunk) => {
                let text = match &content_chunk.content {
                    agent_client_protocol::ContentBlock::Text(text_content) => {
                        text_content.text.clone()
                    }
                    agent_client_protocol::ContentBlock::Image(_) => "<image>".into(),
                    agent_client_protocol::ContentBlock::Audio(_) => "<audio>".into(),
                    agent_client_protocol::ContentBlock::ResourceLink(resource_link) => {
                        resource_link.uri.clone()
                    }
                    agent_client_protocol::ContentBlock::Resource(_) => "<resource>".into(),
                    // 处理未来可能添加的新内容类型
                    _ => "<unknown>".into(),
                };
                info!(
                    "📥 Agent message cached [session:{}]: {}",
                    session_id_str, text
                );
            }
            _ => {
                info!(
                    "📥 SessionUpdate cached [session:{}]: {:?}",
                    session_id_str, args.update
                );
            }
        }

        Ok(())
    }

    async fn ext_method(
        &self,
        _request: agent_client_protocol::ExtRequest,
    ) -> Result<agent_client_protocol::ExtResponse, agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }

    async fn ext_notification(
        &self,
        _notification: agent_client_protocol::ExtNotification,
    ) -> Result<(), agent_client_protocol::Error> {
        Err(agent_client_protocol::Error::method_not_found())
    }
}
