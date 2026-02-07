//! Computer Chat Handler
//!
//! 处理 POST /computer/chat 请求

use axum::{Json, extract::State, http::StatusCode};
use std::sync::Arc;
use tracing::{error, info};

use crate::http_server::router::AppState;
use crate::service::AGENT_REGISTRY;
use crate::service::chat_handler::{ChatHandlerContext, ChatHandlerInput, handle_chat_core};
use crate::service::{SESSION_CACHE, SessionData};
use dashmap::mapref::entry::Entry;
use shared_types::{ChatResponse, ComputerChatRequest, HttpResult, ServiceType};

/// 处理 Computer Agent Chat 请求
///
/// 直接调用 agent_runner 本地的 handle_chat_core(),无需 gRPC 转发
#[utoipa::path(
    post,
    path = "/computer/chat",
    request_body = ComputerChatRequest,
    responses(
        (status = 200, description = "Chat request successful", body = HttpResult<ChatResponse>),
        (status = 400, description = "Bad request - missing user_id"),
        (status = 500, description = "Internal server error")
    ),
    tag = "Computer Agent"
)]
pub async fn handle_computer_chat(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ComputerChatRequest>,
) -> Result<Json<HttpResult<ChatResponse>>, (StatusCode, Json<HttpResult<ChatResponse>>)> {
    info!(
        "📨 [HTTP] 收到 Computer Chat 请求: user_id={:?}, project_id={:?}, session_id={:?}, prompt_len={}, attachments={}, has_model_config={}, has_agent_config={}",
        request.user_id,
        request.project_id,
        request.session_id,
        request.prompt.len(),
        request.attachments.len(),
        request.model_provider.is_some(),
        request.agent_config.is_some()
    );
    info!("📝 [HTTP] 请求详情: prompt={:?}", request.prompt);

    // 1. 验证必填字段
    if request.user_id.is_empty() {
        let error_msg = "user_id is required for ComputerAgentRunner";
        error!("❌ [HTTP] {}", error_msg);
        return Err((
            StatusCode::BAD_REQUEST,
            Json(HttpResult::error("VALIDATION_ERROR", error_msg)),
        ));
    }

    let user_id = request.user_id.clone();

    // 2. 生成或使用提供的 project_id (直接用 UUID，去掉连字符，与 rcoder 保持一致)
    let project_id = request
        .project_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string().replace('-', ""));

    // 3. 自动查找现有 session_id (如果未提供)
    let session_id = request.session_id.or_else(|| {
        AGENT_REGISTRY
            .get_agent_info(&project_id)
            .map(|info| info.session_id.to_string())
    });

    // 4. 创建项目工作目录（使用配置中的 projects_dir，支持外部配置）
    let project_dir = state.config.projects_dir.join(&user_id).join(&project_id);

    if let Err(e) = tokio::fs::create_dir_all(&project_dir).await {
        let error_msg = format!("Failed to create project directory: {}", e);
        error!("❌ [HTTP] {}", error_msg);
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(HttpResult::error("INTERNAL_ERROR", &error_msg)),
        ));
    }

    // 5. 生成或使用提供的 request_id
    let request_id = request
        .request_id
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // 6. 构建 ChatHandlerInput
    let input = ChatHandlerInput {
        project_id: project_id.clone(),
        project_dir,
        session_id,
        prompt: request.prompt,
        request_id: request_id.clone(),
        attachments: request.attachments,
        data_source_attachments: request.data_source_attachments,
        model_config: request.model_provider,
        service_type: ServiceType::ComputerAgentRunner,
        agent_config_override: request.agent_config,
        system_prompt_override: request.system_prompt,
        user_prompt_template_override: request.user_prompt,
    };

    // 7. 构建 ChatHandlerContext
    let context = ChatHandlerContext {
        agent_runtime: state.agent_runtime.clone(),
        shared_api_key_manager: state.shared_api_key_manager.clone(),
        project_uuid_map: state.project_uuid_map.clone(),
    };

    // 8. 调用核心 Chat 处理逻辑
    let output = handle_chat_core(input, &context).await;

    // 🔧 关键修复：将 session 写入 SESSION_CACHE（SSE 进度流需要从这里读取）
    let session_id_str = output.session_id.clone();
    match SESSION_CACHE.entry(session_id_str.clone()) {
        Entry::Occupied(entry) => {
            info!(
                "[HTTP] SESSION_CACHE 已存在，复用: session_id={}",
                session_id_str
            );
            entry.get().clone()
        }
        Entry::Vacant(entry) => {
            let data = SessionData::new(1000);
            info!("[HTTP] SESSION_CACHE 新建: session_id={}", session_id_str);
            entry.insert(data.clone());
            data
        }
    };

    // 9. 构建响应
    let response = ChatResponse {
        project_id: output.project_id.clone(),
        session_id: output.session_id.clone(),
        error: output.error.clone(),
        request_id: Some(request_id),
        need_fallback: None,
        fallback_reason: None,
    };

    // 10. 根据执行结果返回成功或错误
    if output.error.is_some() || !output.success {
        error!(
            "❌ [HTTP] Computer Chat 失败: session_id={}, error={:?}",
            response.session_id, response.error
        );
        // 返回成功的 HTTP 状态码，但 HttpResult 包含错误信息
        // 这与 rcoder 的行为一致：HTTP 200 + HttpResult.error
        return Ok(Json(HttpResult::error(
            output.error_code.as_deref().unwrap_or("CHAT_ERROR"),
            output.error.as_deref().unwrap_or("Unknown error"),
        )));
    }

    info!(
        "✅ [HTTP] Computer Chat 响应: session_id={}, error={:?}",
        response.session_id, response.error
    );

    Ok(Json(HttpResult::success(response)))
}
