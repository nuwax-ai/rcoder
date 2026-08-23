//! 聊天处理器
//!
//! 将原始 HTTP 请求直接转发到容器内的 agent_runner 服务
//!
//! ## 模块组织
//!
//! `handle_chat` 仅负责阶段编排，各阶段拆分为独立函数：
//! - [`validate_and_route_chat_request`]：请求校验与路由解析
//! - [`resolve_container_target`]：容器目标解析（获取/创建容器 + 项目记录维护）
//! - 转发编排：安装检查、session 解析、就绪探活、gRPC 转发、响应后状态更新

mod probe;
mod routing;
mod types;
mod validation;

use crate::grpc;
use crate::handler;
use probe::probe_agent_runner_readiness;
use routing::{
    ensure_chat_agent_installed_if_needed, resolve_chat_forward_request, resolve_container_target,
};
use validation::validate_and_route_chat_request;

use types::{ChatRouteTarget, ForwardContext};

use anyhow::Result;
use axum::{extract::State, http::HeaderMap};
use shared_types::{AgentChatRequest, ChatResponse};
use std::sync::Arc;
use tracing::{debug, error, info, instrument, warn};

use docker_manager::ContainerBasicInfo;

use crate::handler::chat_forward::ChatFlowExit;
use crate::handler::utils::{I18nJsonOrQuery, get_locale_from_headers};
use crate::{AppError, HttpResult, router::AppState};

/// 处理聊天请求 - 转发到容器化 agent_runner 服务
///
/// 1. 根据 project_id 检查或动态创建对应的容器（默认使用 ServiceType::WebAgentRunner）
/// 2. 将原始聊天请求直接转发到容器内的 agent_runner 服务
/// 3. 获取并返回 agent_runner 的处理结果
///
/// 注意：
/// - 所有参数处理（如 project_id、session_id 生成）都由 agent_runner 处理
/// - RCoder 只负责容器管理和请求转发
/// - 当前默认使用 ServiceType::WebAgentRunner，AgentRunner 模式正在开发中
/// - Resume 会话的降级逻辑已在 agent_runner 层通过 list_sessions API 预检查处理
#[utoipa::path(
    post,
    path = "/chat",
    request_body(
        content = AgentChatRequest,
        description = "聊天请求，包含用户输入的 prompt 和可选的多媒体附件",
        content_type = "application/json"
    ),
    responses(
        (
            status = 200,
            description = "成功处理聊天请求",
            body = HttpResult<ChatResponse>,
            example = json!({
                "success": true,
                "data": {
                    "project_id": "test_project",
                    "session_id": "session456",
                    "error": null,
                    "request_id": "req_123456789"
                },
                "error": null
            })
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
        ),
        (
            status = 500,
            description = "服务器内部错误或容器服务异常",
            body = HttpResult<String>,
            example = json!({
                "success": false,
                "data": null,
                "error": {
                    "code": "INTERNAL001",
                    "message": "Internal server error"
                }
            })
        )
    ),
    tag = "chat",
    operation_id = "handle_chat",
    summary = "转发聊天消息到容器化 AI 服务",
    description = "根据 project_id 动态管理容器（默认使用 ServiceType::WebAgentRunner），将原始聊天请求直接转发到容器内的 agent_runner 服务进行处理"
)]
#[instrument(skip(state, request), fields(project_id = ?request.project_id, session_id = ?request.session_id))]
pub async fn handle_chat(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(mut request): I18nJsonOrQuery<AgentChatRequest>,
) -> Result<HttpResult<ChatResponse>, AppError> {
    match run_chat_flow(state, headers, &mut request).await {
        Ok(result) => Ok(result),
        Err(ChatFlowExit::Response(response)) => Ok(response),
        Err(ChatFlowExit::Fatal(e)) => Err(e),
    }
}

/// Chat 阶段编排（入口只留编排，各阶段见独立函数）
async fn run_chat_flow(
    state: Arc<AppState>,
    headers: HeaderMap,
    request: &mut AgentChatRequest,
) -> Result<HttpResult<ChatResponse>, ChatFlowExit> {
    // 获取语言设置
    let locale = get_locale_from_headers(&headers);

    // 第一段：请求校验与路由解析
    let ChatRouteTarget {
        project_id,
        work_dir_id: _work_dir_id,
        container_work_path,
    } = validate_and_route_chat_request(request, locale)?;

    // 第二段：容器目标解析（获取/创建容器 + 项目记录维护）
    let service_type = shared_types::ServiceType::WebAgentRunner;
    let container_info = resolve_container_target(
        &state,
        request,
        &project_id,
        &container_work_path,
        &service_type,
    )
    .await?;

    // 第三段：转发编排（安装检查 → session 解析 → 就绪探活 → 转发 → 响应后状态更新）
    // 自动安装检查：如果 agent_server 携带 platforms，必须同时提供 agent_id、command、version
    ensure_chat_agent_installed_if_needed(&state, request, &project_id, locale, &service_type)
        .await?;

    // 第三段·会话：自动查找 session_id 并克隆 request 用于转发
    let request_for_forward = resolve_chat_forward_request(&state, request, &project_id);

    // 第三段·探活：agent_runner gRPC 就绪探测（长冷启动下 dial 前置）
    probe_agent_runner_readiness(&state, &container_info, &project_id, locale).await;

    // 第三段·转发：请求发往容器服务（全局连接池）
    info!("[CHAT] Forwarding request to container service");
    let ctx = ForwardContext {
        grpc_pool: &state.grpc_pool,
        namespace: &state.config.app_manager.namespace,
        cluster_domain: &state.cluster_domain,
        runtime: state.runtime(),
        locale,
    };
    let result =
        forward_request_to_container_service(&request_for_forward, &container_info, &ctx).await;
    info!(
        "[CHAT] Container request completed: success={}",
        result.is_ok()
    );

    // 响应后状态更新
    update_session_mapping_from_response(&state, &result, &project_id);

    if result.as_ref().map_or(true, |r| {
        !r.is_success() && r.data.as_ref().is_none_or(|d| d.session_id.is_empty())
    }) {
        error!("[CHAT] Container returned error: {:?}", result);
    }

    info!("[CHAT] Request completed: project_id={}", project_id);

    result.map_err(ChatFlowExit::Fatal)
}

/// 响应后状态更新 - 使用存储
/// 无论请求成功还是失败，只要响应中包含 session_id，都要更新映射
/// 这样用户可以通过 SSE 接口获取错误通知，而不会收到 SESSION_EXPIRED 错误
fn update_session_mapping_from_response(
    state: &Arc<AppState>,
    result: &Result<HttpResult<ChatResponse>, AppError>,
    project_id: &str,
) {
    let Ok(http_result) = result else {
        return;
    };
    let Some(chat_response) = &http_result.data else {
        return;
    };

    let session_id = chat_response.session_id.clone();

    // 只有当 session_id 非空时才更新映射
    if session_id.is_empty() {
        return;
    }

    info!(
        "📊 [CHAT] Received chat response, starting state update: session_id={}, success={}",
        session_id,
        http_result.is_success()
    );

    // C1 修复：用 add_session_to_project 走多 session 单步原子路径，
    // 取代历史非原子的 update_session（write_session_index + entry 两步）。
    // 多 session 模型：一个 project 可同时持有多条活跃 session（多窗口场景）。
    let added = state.add_session_to_project(project_id, &session_id);
    if !added {
        warn!(
            "[CHAT] Project missing during session association, may have been concurrently removed: project_id={}, session_id={}",
            project_id, session_id
        );
    }

    info!(
        "🔗 [SESSION_MAP] Associated session_id {} to project_id {}",
        session_id, project_id
    );

    if http_result.is_success() {
        info!(
            "🎯 [CHAT] All state updates completed: project_id={}, session_id={}",
            project_id, session_id
        );
    } else {
        warn!(
            "⚠️ [CHAT] Request failed but session mapping saved: project_id={}, session_id={}, code={}, message={}",
            project_id, session_id, http_result.code, http_result.message
        );
    }
}

/// 转发请求到容器内的 agent_runner 服务
///
/// 🎯 使用 gRPC Chat RPC 替代 HTTP 转发（使用全局连接池）
async fn forward_request_to_container_service(
    request: &AgentChatRequest,
    container_info: &ContainerBasicInfo,
    ctx: &ForwardContext<'_>,
) -> Result<HttpResult<ChatResponse>, AppError> {
    let project_id = if let Some(id) = &request.project_id {
        id.clone()
    } else {
        error!("[FORWARD]session project_id is required");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            ctx.locale,
        ));
    };

    info!(
        "📤 [FORWARD] Forwarding request to container (gRPC): project_id={}, session_id={:?}, container_id={}, service_url={}",
        project_id, request.session_id, container_info.container_id, container_info.service_url
    );

    // 🎯 使用 gRPC 替代 HTTP
    // K8s 用 Service FQDN，Docker 用容器 IP（统一走 shared_types 分发）
    let grpc_addr = shared_types::build_grpc_addr(
        &container_info.container_name,
        &container_info.container_ip,
        ctx.namespace,
        ctx.cluster_domain,
    );

    debug!(
        "📡 [FORWARD] Sending gRPC request to: {}, prompt_length={}, attachments_count={}",
        grpc_addr,
        request.prompt.len(),
        request.attachments.len()
    );

    // 调用 gRPC Chat（使用全局连接池，带重试和被动驱逐机制；统一转发器见 chat_forward.rs）
    let result = handler::chat_forward::forward_chat(
        ctx.grpc_pool,
        grpc_addr,
        || grpc::GrpcChatParams {
            project_id: project_id.clone(),
            session_id: request.session_id.clone(),
            prompt: request.prompt.clone(),
            attachments: request.attachments.clone(),
            data_source_attachments: request.data_source_attachments.clone(),
            model_config: request.model_provider.clone(),
            request_id: request.request_id.clone(),
            request_timeout: Some(std::time::Duration::from_secs(
                shared_types::GRPC_CHAT_TIMEOUT_SECS,
            )),
            system_prompt: request.system_prompt.clone(),
            user_prompt: request.user_prompt.clone(),
            agent_config: request.agent_config.clone(),
            service_type: Some(shared_types::ServiceType::WebAgentRunner),
            user_id: None,
            is_devcomputer: false,
            agent_work_dir: request.agent_work_dir.clone(),
        },
        ctx.locale,
        handler::chat_forward::ForwardChatOpts {
            log_tag: "FORWARD",
            retry_delay: None,
            re_resolve: Some(handler::chat_forward::ReResolveCtx {
                runtime: ctx.runtime,
                project_id: &project_id,
                service_type: shared_types::ServiceType::WebAgentRunner,
                namespace: ctx.namespace,
                cluster_domain: ctx.cluster_domain,
            }),
            diagnostic: Some(handler::chat_forward::DiagnosticCtx {
                runtime: ctx.runtime,
                identifier: project_id.clone(),
                service_type: shared_types::ServiceType::WebAgentRunner,
            }),
        },
    )
    .await;

    Ok(result)
}
