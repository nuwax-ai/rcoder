//! gRPC Chat 客户端
//!
//! 通过 gRPC 调用 agent_runner 的 Chat RPC

use crate::grpc::GrpcChannelPool;
use shared_types::ChatAgentConfig;
use shared_types::grpc::{
    CancelRequest, CancelResponse, ChatRequest as GrpcChatRequest, ChatResponse as GrpcChatResponse,
};
use std::sync::Arc;
use tracing::{debug, error, info};

/// 通过 gRPC 发送 Chat 请求到 agent_runner (使用连接池)
pub async fn grpc_chat_with_pool(
    pool: &Arc<GrpcChannelPool>,
    grpc_addr: &str,
    project_id: String,
    session_id: Option<String>,
    prompt: String,
    attachments: Vec<shared_types::Attachment>,
    data_source_attachments: Vec<String>,
    model_config: Option<shared_types::ModelProviderConfig>,
    request_id: Option<String>,
    request_timeout: Option<std::time::Duration>,
    // 新增参数 (v2)
    system_prompt: Option<String>,
    user_prompt: Option<String>,
    agent_config: Option<ChatAgentConfig>,
    service_type: Option<shared_types::ServiceType>,
    user_id: Option<String>, // 新增：用于 ComputerAgentRunner 模式
) -> anyhow::Result<GrpcChatResponse> {
    info!(
        "🚀 [gRPC_CHAT] 发送 Chat 请求 (连接池): addr={}, project_id={}",
        grpc_addr, project_id
    );

    // 使用连接池获取客户端
    let mut client = pool.get_client(grpc_addr).await?;

    // 构建 gRPC 请求
    let grpc_request = GrpcChatRequest {
        project_id,
        session_id: session_id.unwrap_or_default(),
        prompt,
        model_config: model_config.map(super::converters::to_grpc_model_config),
        attachments: attachments
            .into_iter()
            .map(super::converters::to_grpc_attachment)
            .collect(),
        request_id,
        data_source_attachments,
        // 新增字段 (v2)
        system_prompt,
        user_prompt,
        agent_config: agent_config.map(super::converters::to_grpc_chat_agent_config),
        service_type: service_type.map(|st| format!("{:?}", st)),
        user_id, // 传递 user_id
    };

    debug!("📤 [gRPC_CHAT] 发送请求: {:?}", grpc_request);

    // 构建 tonic Request 并设置请求级别超时
    let mut request = tonic::Request::new(grpc_request);

    // ✅ 使用 Tonic 原生 API 设置请求超时
    if let Some(timeout) = request_timeout {
        request.set_timeout(timeout);
        debug!("⏱️ [gRPC_CHAT] 设置请求超时: {:?}", timeout);
    }

    // 发送请求
    let response = client.chat(request).await.map_err(|e| {
        error!("❌ [gRPC_CHAT] Chat RPC 调用失败: {}", e);
        anyhow::anyhow!("gRPC Chat 调用失败: {}", e)
    })?;

    let chat_response = response.into_inner();

    info!(
        "✅ [gRPC_CHAT] 收到响应: project_id={}, session_id={}, success={}",
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
    }
}

/// 通过 gRPC 取消会话（使用连接池）
pub async fn grpc_cancel_session_with_pool(
    pool: &Arc<GrpcChannelPool>,
    grpc_addr: &str,
    session_id: String,
    reason: String,
    project_id: String,
) -> anyhow::Result<CancelResponse> {
    info!(
        "🛑 [gRPC_CANCEL] 发送取消会话请求 (连接池): addr={}, session_id={}, project_id={}",
        grpc_addr, session_id, project_id
    );

    // 使用连接池获取客户端
    let mut client = pool.get_client(grpc_addr).await?;

    // 构建 gRPC 请求
    let grpc_request = CancelRequest {
        session_id,
        reason,
        project_id,
    };

    debug!("📤 [gRPC_CANCEL] 发送请求: {:?}", grpc_request);

    // 发送请求
    let response = client
        .cancel_session(tonic::Request::new(grpc_request))
        .await
        .map_err(|e| {
            error!("❌ [gRPC_CANCEL] CancelSession RPC 调用失败: {}", e);
            anyhow::anyhow!("gRPC CancelSession 调用失败: {}", e)
        })?;

    let cancel_response = response.into_inner();

    info!(
        "✅ [gRPC_CANCEL] 收到响应: success={}, message={:?}",
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
) -> anyhow::Result<CancelResponse> {
    // 创建临时连接池（单次使用）
    let pool = Arc::new(GrpcChannelPool::new());
    grpc_cancel_session_with_pool(&pool, grpc_addr, session_id, reason, project_id).await
}

/// 通过 gRPC 停止 Agent（使用连接池）
pub async fn grpc_stop_agent_with_pool(
    pool: &Arc<GrpcChannelPool>,
    grpc_addr: &str,
    project_id: String,
    reason: Option<String>,
    force: bool,
) -> anyhow::Result<shared_types::grpc::StopAgentResponse> {
    info!(
        "🔄 [gRPC_STOP_AGENT] 发送停止 Agent 请求 (连接池): addr={}, project_id={}, force={}",
        grpc_addr, project_id, force
    );

    // 使用连接池获取客户端
    let mut client = pool.get_client(grpc_addr).await?;

    // 构建 gRPC 请求
    let grpc_request = shared_types::grpc::StopAgentRequest {
        project_id: project_id.clone(),
        reason,
        force,
    };

    debug!("📤 [gRPC_STOP_AGENT] 发送请求: {:?}", grpc_request);

    // 发送请求
    let response = client
        .stop_agent(tonic::Request::new(grpc_request))
        .await
        .map_err(|e| {
            error!("❌ [gRPC_STOP_AGENT] StopAgent RPC 调用失败: {}", e);
            anyhow::anyhow!("gRPC StopAgent 调用失败: {}", e)
        })?;

    let stop_response = response.into_inner();

    info!(
        "✅ [gRPC_STOP_AGENT] 收到响应: result={}, success={}, message={:?}",
        stop_response.result, stop_response.success, stop_response.message
    );

    Ok(stop_response)
}
