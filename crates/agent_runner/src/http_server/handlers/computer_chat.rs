//! Computer Chat Handler
//!
//! 处理 POST /computer/chat 请求

use axum::{Json, extract::State, http::HeaderMap};
use std::sync::Arc;
use tracing::{error, info};

use crate::http_server::router::AppState;
use crate::service::AGENT_REGISTRY;
use crate::service::chat_handler::{ChatHandlerContext, ChatHandlerInput, handle_chat_core};
use crate::service::{SESSION_CACHE, SessionData};
use dashmap::mapref::entry::Entry;
use shared_types::{
    ChatResponse, ComputerChatRequest, HttpResult, I18nJsonOrQuery, ServiceType,
    error_codes::ERR_INTERNAL_SERVER_ERROR, error_codes::ERR_VALIDATION, get_i18n_message,
};

use super::locale_from_headers;

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
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerChatRequest>,
) -> Result<Json<HttpResult<ChatResponse>>, shared_types::AppError> {
    let locale = locale_from_headers(&headers);
    info!(
        "📨 [HTTP] Received Computer Chat request:\n\
         ├─ user_id: {:?}\n\
         ├─ project_id: {:?}\n\
         ├─ session_id: {:?}\n\
         ├─ request_id: {:?}\n\
         ├─ prompt ({}chars): {:?}\n\
         ├─ pod_id: {:?}\n\
         ├─ tenant_id: {:?}\n\
         ├─ space_id: {:?}\n\
         ├─ isolation_type: {:?}\n\
         ├─ attachments: {:?}\n\
         ├─ data_source_attachments: {:?}\n\
         ├─ model_provider: {:#?}\n\
         ├─ agent_config: {:#?}\n\
         ├─ system_prompt: {:?}\n\
         └─ user_prompt: {:?}",
        request.user_id,
        request.project_id,
        request.session_id,
        request.request_id,
        request.prompt.len(),
        request.prompt,
        request.pod_id,
        request.tenant_id,
        request.space_id,
        request.isolation_type,
        request.attachments,
        request.data_source_attachments,
        request.model_provider,
        request.agent_config,
        request.system_prompt,
        request.user_prompt
    );

    // 1. 验证必填字段
    if request.user_id.is_empty() {
        let error_msg = get_i18n_message("error.user_id_required", locale);
        error!("[HTTP] {}", error_msg);
        return Err(shared_types::AppError::with_i18n_key(
            ERR_VALIDATION,
            &error_msg,
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
    // Docker 挂载：宿主机 /computer-project-workspace/{user_id} → 容器 /home/user
    // Agent 工作目录：/home/user/{project_id}
    let project_dir = state.config.projects_dir.join(&project_id);

    if let Err(e) = tokio::fs::create_dir_all(&project_dir).await {
        let error_msg = format!(
            "{}: {}",
            get_i18n_message("error.project_dir_create_failed", locale),
            e
        );
        error!("[HTTP] {}", error_msg);
        return Err(shared_types::AppError::with_message(
            ERR_INTERNAL_SERVER_ERROR,
            &error_msg,
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
        user_id: Some(user_id),
        agent_config_override: request.agent_config,
        system_prompt_override: request.system_prompt,
        user_prompt_template_override: request.user_prompt,
    };

    // 7. 构建 ChatHandlerContext
    let context = ChatHandlerContext {
        agent_session_service: state.agent_session_service.clone(),
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
                "[HTTP] SESSION_CACHE already exists, reusing: session_id={}",
                session_id_str
            );
            entry.get().clone()
        }
        Entry::Vacant(entry) => {
            let data = SessionData::new(1000);
            info!(
                "[HTTP] SESSION_CACHE created: session_id={}",
                session_id_str
            );
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
        reloaded: if output.reloaded { Some(true) } else { None },
    };

    // 10. 根据执行结果返回成功或错误
    if output.error.is_some() || !output.success {
        error!(
            "❌ [HTTP] Computer Chat failed: session_id={}, error={:?}",
            response.session_id, response.error
        );
        // 返回成功的 HTTP 状态码，但 HttpResult 包含错误信息
        // 这与 rcoder 的行为一致：HTTP 200 + HttpResult.error
        return Ok(Json(HttpResult::error(
            output
                .error_code
                .as_deref()
                .unwrap_or(ERR_INTERNAL_SERVER_ERROR),
            &shared_types::get_error_message(
                output
                    .error_code
                    .as_deref()
                    .unwrap_or(ERR_INTERNAL_SERVER_ERROR),
                locale,
            ),
        )));
    }

    info!(
        "✅ [HTTP] Computer Chat response: session_id={}, error={:?}",
        response.session_id, response.error
    );

    Ok(Json(HttpResult::success(response)))
}
