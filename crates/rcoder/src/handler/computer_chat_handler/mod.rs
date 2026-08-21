//! Computer Agent Runner 聊天处理器
//!
//! 处理 Computer Agent Runner 模式的聊天请求。
//! 与 RCoder 的 project_id 容器模式不同，ComputerAgentRunner 使用 user_id 作为容器标识。
//!
//! ## 请求流程
//! ```text
//! POST /computer/chat { user_id, project_id?, prompt, ... }
//!     ↓
//! 1. 验证 user_id
//! 2. 生成 project_id（若未提供）
//! 3. get_or_create_container_for_user(user_id)
//!    - 挂载配置: config.yml mounts (配置化管理)
//!    - 宿主机: /computer-project-workspace/{user_id} → 容器: /home/user
//! 4. 创建项目工作目录: /home/user/{project_id} (通过挂载自动同步)
//! 5. 创建/更新项目和会话信息
//! 6. gRPC Chat RPC → agent_runner (带 project_id)
//! 7. 更新会话映射
//! 8. 返回 ChatResponse
//! ```
//!
//! 注意：Resume 会话的降级逻辑已在 agent_runner 层通过 list_sessions API 预检查处理
//!
//! ## 模块组织
//!
//! - [`validation`]：请求校验与 project_id / work_dir_id 解析
//! - [`container`]：容器就绪/唤醒（并发等待、按需创建、空 IP 修复、映射写入）
//! - [`session`]：Agent 安装检查、VNC 注册、状态探活、session 解析、响应后映射更新
//! - [`forward`]：gRPC 转发
//! - [`helpers`]：工作目录创建与项目映射
//!
//! 入口 `handle_computer_chat_internal` 仅负责阶段编排。

use axum::{extract::State, http::HeaderMap};
use shared_types::{ChatResponse, ComputerChatRequest};
use std::sync::Arc;
use tracing::{debug, error, info, instrument};

use crate::{AppError, HttpResult, router::AppState};
use docker_manager::ContainerBasicInfo;

use super::chat_forward::ChatFlowExit;
use super::utils::{
    I18nJsonOrQuery, build_computer_workspace_path, get_locale_from_headers, project_dir,
};

/// 处理 Computer Agent 聊天请求
///
/// 1. 根据 user_id 获取或创建用户容器
/// 2. 将聊天请求转发到容器内的 agent_runner 服务
/// 3. 更新会话映射
///
/// 注意：
/// - user_id 是必填的，用于标识用户的容器
/// - project_id 可选，若未提供则自动生成
/// - 一个用户容器内可以运行多个 project_id 的 Agent 实例
/// - Resume 会话的降级逻辑已在 agent_runner 层通过 list_sessions API 预检查处理
#[utoipa::path(
    post,
    path = "/computer/chat",
    request_body(
        content = ComputerChatRequest,
        description = "Computer Agent 聊天请求，包含 user_id 和 prompt",
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
                    "project_id": "proj_456",
                    "session_id": "session789",
                    "error": null,
                    "request_id": "req_123456789"
                },
                "error": null
            })
        ),
        (
            status = 400,
            description = "请求参数错误（如 user_id 为空）",
            body = HttpResult<String>
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
        ),
        (
            status = 500,
            description = "服务器内部错误",
            body = HttpResult<String>
        )
    ),
    tag = "computer",
    operation_id = "handle_computer_chat",
    summary = "发送聊天消息到 Computer Agent",
    description = "根据 user_id 动态管理容器，一个用户对应一个带桌面环境的容器"
)]
#[instrument(skip(state, request), fields(user_id = %request.user_id, project_id = ?request.project_id))]
pub async fn handle_computer_chat(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerChatRequest>,
) -> Result<HttpResult<ChatResponse>, AppError> {
    handle_computer_chat_internal(State(state), headers, I18nJsonOrQuery(request), false).await
}

/// Computer Chat 内部处理函数
///
/// 支持 `is_devcomputer` 参数，用于区分 `/computer/chat` 和 `/devcomputer/chat` 请求
pub(crate) async fn handle_computer_chat_internal(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerChatRequest>,
    is_devcomputer: bool,
) -> Result<HttpResult<ChatResponse>, AppError> {
    match run_computer_chat_flow(state, headers, request, is_devcomputer).await {
        Ok(result) => Ok(result),
        Err(ChatFlowExit::Response(response)) => Ok(response),
        Err(ChatFlowExit::Fatal(e)) => Err(e),
    }
}

/// userApp 开发对话的 service_type 标记值（`X-Service-Type` 同款词表）。
const SERVICE_TYPE_USERAPP: &str = "userapp";

/// Computer Chat 阶段编排（入口只留编排，各阶段见子模块）
async fn run_computer_chat_flow(
    state: Arc<AppState>,
    headers: HeaderMap,
    mut request: ComputerChatRequest,
    is_devcomputer: bool,
) -> Result<HttpResult<ChatResponse>, ChatFlowExit> {
    // 获取语言设置
    let locale = get_locale_from_headers(&headers);

    // userApp 开发对话分支：service_type=userapp → 该 app 的 UserAppBuilder 开发容器
    // （ACP agent 直接在开发卷 workspace 工作，代码生成直接落卷）
    if request
        .service_type
        .as_deref()
        .is_some_and(|v| v.trim() == SERVICE_TYPE_USERAPP)
    {
        return run_userapp_dev_chat_flow(state, locale, request, is_devcomputer).await;
    }

    // 1~3. 请求校验与路由解析（user_id / 隔离参数 / project_id / work_dir_id / 资源限制）
    let (project_id, work_dir_id) = validation::validate_and_prepare_request(&mut request, locale)?;
    let user_id = request.user_id.clone();

    // 4~5. 容器就绪/唤醒（并发等待、按需创建、空 IP 修复、映射写入、活动时间更新）
    let container_info =
        container::ensure_container_ready(&state, &request, &user_id, &project_id, locale).await?;

    // 自动安装检查：如果 agent_server 携带 platforms，必须同时提供 agent_id、command、version
    session::ensure_agent_installed_if_needed(&state, &request, &project_id, locale).await?;

    // 5. 创建项目工作目录（在用户容器内）
    // Computer Agent Runner 需要在用户工作区内为 work_dir_id 创建子目录
    // 使用 ? 传播 AppError：验证错误 → HTTP 400，I/O 错误 → HTTP 500
    helpers::ensure_project_workspace_exists(
        request.isolation_type.as_deref(),
        request.tenant_id.as_deref(),
        request.space_id.as_deref(),
        &user_id,
        &work_dir_id,
    )
    .await?;

    // 6. 注册 VNC 后端到 Pingora（用于 WebSocket 代理）
    session::register_vnc_backend(&state, &user_id, &container_info);

    // 6.5. 🆕 主动查询 Agent 状态 (User Request)
    session::probe_agent_status(&state, &container_info, &project_id, locale).await;

    // 7. 🆕 自动查找 session_id 逻辑（克隆 request 并注入解析后的 session_id）
    let request_for_forward = session::resolve_forward_request(&state, &request, &project_id);

    // 8. 转发请求到容器服务（使用 gRPC）
    let forward_params = forward::ComputerForwardParams {
        request: &request_for_forward,
        project_id: &project_id,
        work_dir_id: &work_dir_id,
        container_info: &container_info,
        grpc_pool: &state.grpc_pool,
        locale,
        is_devcomputer,
        namespace: &state.config.app_manager.namespace,
        cluster_domain: &state.cluster_domain,
        runtime: state.runtime(),
        service_type: shared_types::ServiceType::ComputerAgentRunner,
        diagnostic_identifier: user_id.clone(),
    };
    let result = forward::forward_computer_request_to_container(forward_params).await;

    // 8. 更新会话映射（填充所有三个映射表，保持一致性）
    session::update_session_mappings_after_response(
        &state,
        &result,
        &user_id,
        &project_id,
        &container_info,
        &request,
        &shared_types::ServiceType::ComputerAgentRunner,
    )
    .await?;

    if !result.is_success() && result.data.as_ref().is_none_or(|d| d.session_id.is_empty()) {
        error!(
            "❌ [COMPUTER_CHAT] Container service returned error (no session_id): user_id={}, project_id={}, code={}, message={}",
            user_id, project_id, result.code, result.message
        );
    }

    Ok(result)
}

/// userApp 开发对话流程：project_id 必填=app_id，容器=该 app 的 UserAppBuilder
/// 开发容器（per-app），workspace={USERAPP_WORKSPACE_DIR}/{app_id}（容器内）。
///
/// 与普通 computer 链路的差异：
/// - 容器 ensure 走 `ensure_userapp_builder`（幂等，注册 state.projects 防孤立清理）
/// - workspace 目录由容器内 file-server `ensure-workspace` 幂等创建（rcoder 无共享卷）
/// - 跳过 VNC 注册（开发容器无桌面代理需求；devapps 可代理任意端口兜底）
/// - 跳过 agent 自动安装（UserAppBuilder 安装策略 None）
/// - gRPC service_type=UserAppBuilder → agent_runner work_dir 命中开发卷分支
async fn run_userapp_dev_chat_flow(
    state: Arc<AppState>,
    locale: &'static str,
    request: ComputerChatRequest,
    is_devcomputer: bool,
) -> Result<HttpResult<ChatResponse>, ChatFlowExit> {
    // 1. 校验：user_id 必填 + project_id 必填（=app_id，不自动生成——app 语义明确）
    if request.user_id.trim().is_empty() {
        return Err(ChatFlowExit::Response(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        )));
    }
    let project_id = match request.project_id.as_deref() {
        Some(id) if !id.trim().is_empty() => id.trim().to_string(),
        _ => {
            return Err(ChatFlowExit::Response(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                "project_id (= app_id) is required for userApp dev chat",
            )));
        }
    };
    if let Err(e) = shared_types::validate_identifier(&project_id, "project_id") {
        return Err(ChatFlowExit::Response(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            &e,
        )));
    }
    let work_dir_id = project_id.clone();
    let user_id = request.user_id.clone();
    info!(
        "🚀 [USERAPP_DEV_CHAT] user_id={}, app_id={}, session_id={:?}, prompt_len={}",
        user_id,
        project_id,
        request.session_id,
        request.prompt.len()
    );

    // 2. 容器 ensure（幂等；注册 state.projects）+ 活动时间刷新（防对话中被闲置回收）
    let container_info =
        crate::userapp_publish::agent_runner::ensure_userapp_builder(&state, &project_id)
            .await
            .map_err(|e| {
                error!(
                    "❌ [USERAPP_DEV_CHAT] ensure dev container failed: app_id={}: {e:#}",
                    project_id
                );
                ChatFlowExit::Response(HttpResult::error_with_locale(
                    shared_types::error_codes::ERR_CONTAINER_ERROR,
                    locale,
                ))
            })?;
    state.update_activity(&project_id);

    // 3. workspace 就绪（容器内幂等建目录）
    ensure_dev_workspace(&state, &container_info, &project_id, &user_id, locale).await?;

    // 4. Agent 状态探活 + session 解析（复用 computer 实现，按 project_id 映射通用）
    session::probe_agent_status(&state, &container_info, &project_id, locale).await;
    let request_for_forward = session::resolve_forward_request(&state, &request, &project_id);

    // 5. gRPC 转发（service_type=UserAppBuilder → agent_runner 开发卷 work_dir）
    let forward_params = forward::ComputerForwardParams {
        request: &request_for_forward,
        project_id: &project_id,
        work_dir_id: &work_dir_id,
        container_info: &container_info,
        grpc_pool: &state.grpc_pool,
        locale,
        is_devcomputer,
        namespace: &state.config.app_manager.namespace,
        cluster_domain: &state.cluster_domain,
        runtime: state.runtime(),
        service_type: shared_types::ServiceType::UserAppBuilder,
        diagnostic_identifier: project_id.clone(),
    };
    let result = forward::forward_computer_request_to_container(forward_params).await;

    // 6. 会话映射更新（service_type=UserAppBuilder；session→project 映射供 SSE/会话族接口路由）
    session::update_session_mappings_after_response(
        &state,
        &result,
        &user_id,
        &project_id,
        &container_info,
        &request,
        &shared_types::ServiceType::UserAppBuilder,
    )
    .await?;

    if !result.is_success() && result.data.as_ref().is_none_or(|d| d.session_id.is_empty()) {
        error!(
            "❌ [USERAPP_DEV_CHAT] Container service returned error (no session_id): app_id={}, code={}, message={}",
            project_id, result.code, result.message
        );
    }

    Ok(result)
}

/// 容器内幂等建 workspace 目录（file-server `ensure-workspace`）。
async fn ensure_dev_workspace(
    state: &AppState,
    container_info: &ContainerBasicInfo,
    app_id: &str,
    user_id: &str,
    locale: &'static str,
) -> Result<(), ChatFlowExit> {
    use crate::userapp_publish::agent_runner::dev_file_server_addr;
    let addr = dev_file_server_addr(state, container_info);
    let resp = crate::http_client::shared_client()
        .post(format!("{addr}/api/userapp/ensure-workspace"))
        .json(&serde_json::json!({"appId": app_id, "userId": user_id}))
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            error!(
                "❌ [USERAPP_DEV_CHAT] ensure-workspace returned {status}: app_id={app_id}: {text}"
            );
            Err(ChatFlowExit::Response(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_CONTAINER_ERROR,
                locale,
            )))
        }
        Err(e) => {
            error!("❌ [USERAPP_DEV_CHAT] ensure-workspace request failed: app_id={app_id}: {e}");
            Err(ChatFlowExit::Response(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_CONTAINER_ERROR,
                locale,
            )))
        }
    }
}

// computer_chat_handler 目录化：按阶段拆分子模块
mod container;
mod forward;
mod helpers;
mod session;
mod validation;
