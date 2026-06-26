//! gRPC Chat 客户端
//!
//! 通过 gRPC 调用 agent_runner 的 Chat RPC

use crate::grpc::{GrpcChannelPool, GrpcError};
use shared_types::ChatAgentConfig;
use shared_types::grpc::{
    CancelRequest, CancelResponse, ChatRequest as GrpcChatRequest,
    ChatResponse as GrpcChatResponse, ResolvePermissionRequest as GrpcResolvePermissionRequest,
    ResolvePermissionResponse as GrpcResolvePermissionResponse,
};
use std::sync::Arc;
use tonic::Status;
use tracing::{debug, error, info};

/// gRPC Chat 请求参数
///
/// 封装了发送 Chat 请求到 agent_runner 所需的所有参数，
/// 避免函数参数过多，同时支持不同场景的扩展。
pub struct GrpcChatParams {
    /// 项目 ID
    pub project_id: String,
    /// 会话 ID（可选）
    pub session_id: Option<String>,
    /// 用户提示
    pub prompt: String,
    /// 附件列表
    pub attachments: Vec<shared_types::Attachment>,
    /// 数据源附件列表
    pub data_source_attachments: Vec<String>,
    /// 模型配置
    pub model_config: Option<shared_types::ModelProviderConfig>,
    /// 请求 ID
    pub request_id: Option<String>,
    /// 请求超时时间
    pub request_timeout: Option<std::time::Duration>,
    /// 系统提示（v2）
    pub system_prompt: Option<String>,
    /// 用户提示（v2）
    pub user_prompt: Option<String>,
    /// Agent 配置
    pub agent_config: Option<ChatAgentConfig>,
    /// 服务类型
    pub service_type: Option<shared_types::ServiceType>,
    /// 用户 ID（用于 ComputerAgentRunner 模式）
    pub user_id: Option<String>,
    /// 是否是 DevComputer 接口请求
    pub is_devcomputer: bool,
    /// 自定义工作目录标识符
    pub agent_work_dir: Option<String>,
}

/// 通过 gRPC 发送 Chat 请求到 agent_runner (使用连接池)
///
/// 返回 `Result<GrpcChatResponse, GrpcError>`，其中 `GrpcError` 包含
/// 分类后的错误信息，便于调用方判断是否应该重试。
pub async fn grpc_chat_with_pool(
    pool: &Arc<GrpcChannelPool>,
    grpc_addr: &str,
    params: GrpcChatParams,
) -> Result<GrpcChatResponse, GrpcError> {
    info!(
        " [gRPC_CHAT] Sending Chat request (connection pool): addr={}, project_id={}, is_devcomputer={}",
        grpc_addr, params.project_id, params.is_devcomputer
    );

    // 使用连接池获取客户端
    let mut client = pool.get_client(grpc_addr).await.map_err(|e| {
        error!("[gRPC_CHAT] Failed to get client: {}", e);
        // 连接池错误通常是连接层问题，使用 Status::unavailable 表示
        GrpcError::Status(Status::unavailable(format!("Connection failed: {}", e)))
    })?;

    // 构建 gRPC 请求
    let grpc_request = GrpcChatRequest {
        project_id: params.project_id,
        session_id: params.session_id.unwrap_or_default(),
        prompt: params.prompt,
        model_config: params.model_config.map(super::converters::to_grpc_model_config),
        attachments: params
            .attachments
            .into_iter()
            .map(super::converters::to_grpc_attachment)
            .collect(),
        request_id: params.request_id,
        data_source_attachments: params.data_source_attachments,
        // 新增字段 (v2)
        system_prompt: params.system_prompt,
        user_prompt: params.user_prompt,
        agent_config: params.agent_config.map(super::converters::to_grpc_chat_agent_config),
        service_type: params.service_type.map(|st| st.to_string()),
        user_id: params.user_id,        // 传递 user_id
        is_devcomputer: params.is_devcomputer, // 🆕 传递 is_devcomputer
        agent_work_dir: params.agent_work_dir, // 🆕 传递 agent_work_dir
    };

    debug!("[gRPC_CHAT] sendrequest: {:?}", grpc_request);

    // 构建 tonic Request 并设置请求级别超时
    let locale = super::current_grpc_locale();
    let mut request = super::new_request_with_locale(grpc_request, locale);

    // ✅ 使用 Tonic 原生 API 设置请求超时
    if let Some(timeout) = params.request_timeout {
        request.set_timeout(timeout);
        debug!(" [gRPC_CHAT] request timeout: {:?}", timeout);
    }

    // 发送请求，使用 GrpcError 包装错误
    let response = client.chat(request).await.map_err(|e| {
        error!("[gRPC_CHAT] Chat RPC call failed: {}", e);
        GrpcError::from(e)
    })?;

    let chat_response = response.into_inner();

    info!(
        " [gRPC_CHAT] Received response: project_id={}, session_id={}, success={}",
        chat_response.project_id, chat_response.session_id, chat_response.success
    );

    Ok(chat_response)
}

/// 将 gRPC ChatResponse 转换为内部 ChatResponse
pub fn grpc_response_to_chat_response(grpc_resp: GrpcChatResponse) -> shared_types::ChatResponse {
    shared_types::ChatResponse {
        project_id: grpc_resp.project_id,
        session_id: grpc_resp.session_id,
        error: grpc_resp.error,
        request_id: grpc_resp.request_id,
        need_fallback: None,
        fallback_reason: None,
        reloaded: if grpc_resp.reloaded { Some(true) } else { None },
        agent_version: grpc_resp.agent_version,
    }
}

/// 通过 gRPC 取消会话（使用连接池）
pub async fn grpc_cancel_session_with_pool(
    pool: &Arc<GrpcChannelPool>,
    grpc_addr: &str,
    session_id: String,
    reason: String,
    project_id: String,
    request_timeout: Option<std::time::Duration>,
) -> Result<CancelResponse, GrpcError> {
    info!(
        " [gRPC_CANCEL] Sending cancel session request (connection pool): addr={}, session_id={}, project_id={}",
        grpc_addr, session_id, project_id
    );

    // 使用连接池获取客户端
    let mut client = pool.get_client(grpc_addr).await.map_err(|e| {
        error!("[gRPC_CANCEL] Failed to get client: {}", e);
        GrpcError::Status(Status::unavailable(format!("Connection failed: {}", e)))
    })?;

    // 构建 gRPC 请求
    let grpc_request = CancelRequest {
        session_id,
        reason,
        project_id,
    };

    debug!("[gRPC_CANCEL] sendrequest: {:?}", grpc_request);

    // 构建 tonic Request 并设置请求级别超时
    let locale = super::current_grpc_locale();
    let mut request = super::new_request_with_locale(grpc_request, locale);

    let timeout = request_timeout.unwrap_or_else(|| {
        std::time::Duration::from_secs(shared_types::GRPC_CANCEL_SESSION_TIMEOUT_SECS)
    });
    request.set_timeout(timeout);
    debug!(" [gRPC_CANCEL] request timeout: {:?}", timeout);

    let response = client.cancel_session(request).await.map_err(|e| {
        error!("[gRPC_CANCEL] CancelSession RPC call failed: {}", e);
        GrpcError::from(e)
    })?;

    let cancel_response = response.into_inner();

    info!(
        " [gRPC_CANCEL] Received response: success={}, message={:?}",
        cancel_response.success, cancel_response.message
    );

    Ok(cancel_response)
}

/// 通过 gRPC 取消会话（不使用连接池，兼容旧接口）
pub async fn grpc_cancel_session(
    grpc_addr: &str,
    session_id: String,
    reason: String,
    project_id: String,
) -> Result<CancelResponse, GrpcError> {
    // 创建临时连接池（单次使用）
    let pool = Arc::new(GrpcChannelPool::new());
    grpc_cancel_session_with_pool(&pool, grpc_addr, session_id, reason, project_id, None).await
}

/// Resolve a pending ACP permission request through agent_runner gRPC.
pub async fn grpc_resolve_permission_with_pool(
    pool: &Arc<GrpcChannelPool>,
    grpc_addr: &str,
    input: shared_types::ResolvePermissionRequestDto,
    request_timeout: Option<std::time::Duration>,
) -> Result<GrpcResolvePermissionResponse, GrpcError> {
    info!(
        "[gRPC_PERMISSION] Sending ResolvePermission: addr={}, session_id={}, tool_call_id={}",
        grpc_addr, input.session_id, input.tool_call_id
    );

    let mut client = pool.get_client(grpc_addr).await.map_err(|e| {
        error!("[gRPC_PERMISSION] Failed to get client: {}", e);
        GrpcError::Status(Status::unavailable(format!("Connection failed: {}", e)))
    })?;

    let grpc_request = GrpcResolvePermissionRequest {
        session_id: input.session_id,
        tool_call_id: input.tool_call_id,
        option_id: input.option_id,
        cancelled: input.cancelled,
        save_rule: input.save_rule,
        project_id: input.project_id.unwrap_or_default(),
        user_id: input.user_id,
        pod_id: input.pod_id,
        tenant_id: input.tenant_id,
        space_id: input.space_id,
        isolation_type: input.isolation_type,
    };

    let locale = super::current_grpc_locale();
    let mut request = super::new_request_with_locale(grpc_request, locale);

    let timeout = request_timeout.unwrap_or_else(|| {
        std::time::Duration::from_secs(shared_types::GRPC_RESOLVE_PERMISSION_TIMEOUT_SECS)
    });
    request.set_timeout(timeout);
    debug!(" [gRPC_PERMISSION] request timeout: {:?}", timeout);

    let response = client.resolve_permission(request).await.map_err(|e| {
        error!("[gRPC_PERMISSION] ResolvePermission RPC call failed: {}", e);
        GrpcError::from(e)
    })?;

    Ok(response.into_inner())
}

/// 通过 gRPC 停止 Agent（使用连接池）
pub async fn grpc_stop_agent_with_pool(
    pool: &Arc<GrpcChannelPool>,
    grpc_addr: &str,
    project_id: String,
    reason: Option<String>,
    force: bool,
    request_timeout: Option<std::time::Duration>,
) -> Result<shared_types::grpc::StopAgentResponse, GrpcError> {
    info!(
        " [gRPC_STOP_AGENT] Sending stop Agent request (connection pool): addr={}, project_id={}, force={}",
        grpc_addr, project_id, force
    );

    // 使用连接池获取客户端
    let mut client = pool.get_client(grpc_addr).await.map_err(|e| {
        error!("[gRPC_STOP_AGENT] Failed to get client: {}", e);
        GrpcError::Status(Status::unavailable(format!("Connection failed: {}", e)))
    })?;

    // 构建 gRPC 请求
    let grpc_request = shared_types::grpc::StopAgentRequest {
        project_id: project_id.clone(),
        reason,
        force,
    };

    debug!("[gRPC_STOP_AGENT] sendrequest: {:?}", grpc_request);

    // 构建 tonic Request 并设置请求级别超时
    let locale = super::current_grpc_locale();
    let mut request = super::new_request_with_locale(grpc_request, locale);

    let timeout = request_timeout.unwrap_or_else(|| {
        std::time::Duration::from_secs(shared_types::GRPC_STOP_AGENT_TIMEOUT_SECS)
    });
    request.set_timeout(timeout);
    debug!(" [gRPC_STOP_AGENT] request timeout: {:?}", timeout);

    let response = client.stop_agent(request).await.map_err(|e| {
        error!("[gRPC_STOP_AGENT] StopAgent RPC call failed: {}", e);
        GrpcError::from(e)
    })?;

    let stop_response = response.into_inner();

    info!(
        " [gRPC_STOP_AGENT] Received response: result={}, success={}, message={:?}",
        stop_response.result, stop_response.success, stop_response.message
    );

    Ok(stop_response)
}
