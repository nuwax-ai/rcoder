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
    I18nJsonOrQuery, build_computer_workspace_path, get_locale_from_headers,
    map_container_work_dir_to_host, project_dir,
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
            description = "聊天结果信封；已受理任务的会话登记失败返回 success=false，保留 data.session_id，仅重连进度，不重发原消息",
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
            status = 409,
            description = "派发前归属冲突返回 ERR_COMPUTER_OWNER_CONFLICT，reason 区分 user/pod/service_family，retryable=false，消息未发送；登记退休或换代也返回 409。已受理任务的登记错误沿 HTTP 200 失败信封保留 session_id",
            body = HttpResult<ChatResponse>
        ),
        (
            status = 503,
            description = "派发前登记暂未确认；已受理任务的登记错误沿 HTTP 200 失败信封保留 session_id，仅重连进度",
            body = HttpResult<ChatResponse>
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
    description = "Computer 场景按 user_id 管理容器；UserApp 场景按 app_id 定位共享开发容器，project_id 保留独立 agent 会话身份。历史 session 无法复用时自动新建。"
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
        Err(ChatFlowExit::Response(boxed)) => Ok(*boxed),
        Err(ChatFlowExit::Fatal(e)) => Err(e),
    }
}

/// Computer Chat 阶段编排（入口只留编排，各阶段见子模块）
async fn run_computer_chat_flow(
    state: Arc<AppState>,
    headers: HeaderMap,
    mut request: ComputerChatRequest,
    is_devcomputer: bool,
) -> Result<HttpResult<ChatResponse>, ChatFlowExit> {
    // 获取语言设置
    let locale = get_locale_from_headers(&headers);

    // wire 契约防护：app_id 是 userApp 分派定位键，仅在 service_type=userapp
    // 下合法——只传 app_id 不传 service_type 会静默走 computer 路径（project_id
    // 自动生成 UUID、app_id 被丢弃跑错沙箱），fail-open 显式拒绝
    // （agent 族/pod 族以 app_id 存在即分派无此风险；chat 以枚举分派为主判据）
    let has_app_id = request
        .app_id
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty());
    if has_app_id && request.service_type != Some(shared_types::ServiceType::Userapp) {
        return Err(ChatFlowExit::response(
            super::pod_handler::invalid_app_target_response(
                locale,
                "app_id is only valid with service_type=userapp (computer 路径不消费 app_id)",
            ),
        ));
    }

    // userApp 开发对话分支：service_type=Userapp → 该 app 的 UserappBuilder 开发容器
    // （ACP agent 直接在开发卷 workspace 工作，代码生成直接落卷）。
    // 枚举穷尽：未来加业务域变体时此处编译期提醒补分支。
    if request.service_type == Some(shared_types::ServiceType::Userapp) {
        return run_userapp_dev_chat_flow(state, locale, request, is_devcomputer).await;
    }

    // chat 域路由收窄（fail-fast，不静默回落）：service_type 复用容器族词表
    // （与 pod 族同输入面），但仅 userapp 业务域 + computer 族两形态有 chat
    // 路由。userapp 在 chat 域是业务域标记（非容器形态）——目标容器由
    // app_stage 推导（dev→UserappBuilder），直接传容器形态 userapp-builder
    // 属双头语义，与 pod 族/agent 族"勿传 user-app-builder"同语义拒绝。
    if let Some(st) = request.service_type
        && !matches!(
            st,
            shared_types::ServiceType::ComputerAgentRunner
                | shared_types::ServiceType::ComputerNormalProject
        )
    {
        let hint = if matches!(st, shared_types::ServiceType::UserappBuilder) {
            "use service_type=userapp with app_stage (container form is derived from stage)"
        } else {
            "expected userapp / computer-agent-runner / computer-normal-project"
        };
        return Err(ChatFlowExit::response(
            super::pod_handler::invalid_app_target_response(
                locale,
                &format!("service_type {st} has no chat route ({hint})"),
            ),
        ));
    }

    // 1~3. 请求校验与路由解析（user_id / 隔离参数 / project_id / work_dir_id / 资源限制）
    let (project_id, work_dir_id) = validation::validate_and_prepare_request(&mut request, locale)?;
    let user_id = request.user_id.clone();

    // 4~5. 容器就绪/唤醒（并发等待、按需创建、空 IP 修复、映射写入、活动时间更新）
    let registered =
        container::ensure_container_ready(&state, &request, &user_id, &project_id, locale).await?;
    // Project clones normally share ContainerEntry. Detach the exact physical
    // snapshot before awaiting RPC work so a replacement cannot rewrite the
    // executor identity used to append this request's session later.
    let dispatched_project = session::capture_dispatch_project(&registered)?;
    let container_info = dispatched_project.container_info().ok_or_else(|| {
        AppError::internal_server_error("Registered Computer container is missing")
    })?;

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
    let request_for_forward = session::resolve_forward_request(&request, &dispatched_project);

    // 8. 转发请求到容器服务（使用 gRPC）
    // 常规项目走同一 computer 流程（共享容器/ensure/VNC 全复用）；service_type
    // 已是容器族 ServiceType——显式 ComputerAgentRunner 归一（缺省与显式同值），
    // agent_runner cwd 链按 ComputerNormalProject 推导
    let forward_service_type = match request.service_type {
        Some(shared_types::ServiceType::ComputerNormalProject) => {
            shared_types::ServiceType::ComputerNormalProject
        }
        _ => shared_types::ServiceType::ComputerAgentRunner,
    };
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
        service_type: forward_service_type,
        diagnostic_identifier: user_id.clone(),
        force_new_session: false,
    };
    let mut result = forward::forward_computer_request_to_container(forward_params).await;

    // 8. 更新会话映射（填充所有三个映射表，保持一致性）
    session::update_session_mappings_after_response(
        &state,
        &mut result,
        &dispatched_project,
        locale,
    )
    .await;

    if !result.is_success() && result.data.as_ref().is_none_or(|d| d.session_id.is_empty()) {
        error!(
            "❌ [COMPUTER_CHAT] Container service returned error (no session_id): user_id={}, project_id={}, code={}, message={}",
            user_id, project_id, result.code, result.message
        );
    }

    Ok(result)
}

// computer_chat_handler 目录化：按阶段拆分子模块
mod container;
mod forward;
mod helpers;
mod session;
mod userapp;
mod userapp_workspace;

use userapp::run_userapp_dev_chat_flow;
mod validation;
