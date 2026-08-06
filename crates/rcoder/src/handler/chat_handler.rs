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

use anyhow::Result;
use axum::{extract::State, http::HeaderMap};
use shared_types::{AgentChatRequest, IsolationType, ProjectAndContainerInfo};
use std::str::FromStr;
use std::sync::Arc;
use tracing::{debug, error, info, instrument, warn};

use crate::{router::AppState, *};
use docker_manager::ContainerBasicInfo;

use super::chat_forward::ChatFlowExit;
use super::pod_handler::resolve_resource_limits_from_config;

use super::utils::{I18nJsonOrQuery, build_workspace_path, get_locale_from_headers};

/// 转发请求的上下文参数
///
/// 封装了转发请求到容器服务所需的所有参数，
/// 避免函数参数过多，同时支持不同 ServiceType 的业务场景。
///
/// 注意：runtime、rcoder_prefix、computer_prefix 字段
/// 当前在 forward_request_to_container_service 中未使用，
/// 但保留用于未来扩展不同 ServiceType 的业务逻辑。
#[allow(dead_code)]
struct ForwardContext<'a> {
    /// gRPC 连接池
    grpc_pool: &'a Arc<grpc::GrpcChannelPool>,
    /// K8s namespace
    namespace: &'a str,
    /// K8s 集群域名
    cluster_domain: &'a str,
    /// 容器运行时（用于不同 ServiceType 的容器管理）
    runtime: &'a Arc<dyn container_runtime_api::ContainerRuntime>,
    /// RCoder 容器前缀（用于 WebAgentRunner 场景）
    rcoder_prefix: &'a str,
    /// Computer 容器前缀（用于 ComputerAgentRunner 场景）
    computer_prefix: &'a str,
    /// 语言设置
    locale: &'static str,
}

/// 请求校验与路由解析的结果
struct ChatRouteTarget {
    /// 项目 ID（缺省时自动生成）
    project_id: String,
    /// 工作目录标识符（agent_work_dir 或 project_id）
    work_dir_id: String,
    /// 容器内工作空间路径
    container_work_path: String,
}

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

    // 🆕 自动查找 session_id 并克隆 request 用于转发
    let request_for_forward = resolve_chat_forward_request(&state, request, &project_id);

    // 2.5 主动探测 agent_runner gRPC 是否就绪
    probe_agent_runner_readiness(&state, &container_info, &project_id, locale).await;

    // 第三步：转发请求到容器服务（使用全局连接池）
    info!("[CHAT] Forwarding request to container service");
    let ctx = ForwardContext {
        grpc_pool: &state.grpc_pool,
        namespace: &state.config.app_manager.namespace,
        cluster_domain: &state.cluster_domain,
        runtime: state.runtime(),
        rcoder_prefix: &state.container_prefix_rcoder,
        computer_prefix: &state.container_prefix_computer,
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

/// 第一段：请求校验与路由解析
///
/// 包含：project_id 生成、work_dir_id 解析与校验、隔离类型参数校验、
/// 工作空间路径构建、资源限制校验。
#[allow(clippy::result_large_err)]
fn validate_and_route_chat_request(
    request: &mut AgentChatRequest,
    locale: &'static str,
) -> Result<ChatRouteTarget, ChatFlowExit> {
    let project_id = match &request.project_id {
        Some(id) => id.clone(),
        None => {
            let project_id = service::container_manager::generate_project_id();
            request.project_id = Some(project_id.clone()); // 设置 project_id
            project_id
        }
    };

    // 确定用于拼接工作目录的标识符
    // agent_work_dir 用于替代 project_id 参与工作目录路径拼接
    let work_dir_id = request
        .agent_work_dir
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| project_id.clone());

    // 校验 work_dir_id（无论来源，用于路径拼接的标识符都应校验）
    if let Err(e) = shared_types::validate_identifier(&work_dir_id, "agent_work_dir") {
        return Err(ChatFlowExit::Response(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            &e,
        )));
    }

    // ========== 隔离类型参数校验 ==========
    // IF pod_id IS NOT NULL THEN isolation_type, tenant_id, space_id 必须非空
    validate_chat_isolation_params(request, locale)?;

    // ========== 构建工作空间路径 ==========
    // 根据 isolation_type 确定容器内工作目录：
    // - tenant/space: /app/project_workspace/{tenant_id}/{space_id}/{work_dir_id}
    // - project 或默认: /app/project_workspace/{work_dir_id}
    let container_work_path = build_workspace_path(
        request.isolation_type.as_deref(),
        request.tenant_id.as_deref(),
        request.space_id.as_deref(),
        &work_dir_id,
    )
    .map_err(|e| AppError::validation_error(&e.to_string()))?;

    info!(
        "📁 [CHAT] Workspace path determined: {} (isolation_type={})",
        container_work_path,
        request.isolation_type.as_deref().unwrap_or("project")
    );

    // 验证资源限制配置
    if let Some(ref agent_config) = request.agent_config
        && let Some(ref resource_limits) = agent_config.resource_limits
    {
        resource_limits
            .validate()
            .map_err(|e| AppError::validation_error(&format!("Invalid resource limits: {}", e)))?;
    }

    info!(
        "🚀 [CHAT] Starting to process chat request: project_id={}, session_id={:?}, prompt_length={}, attachments_count={}, model_provider={}",
        project_id,
        request.session_id,
        request.prompt.len(),
        request.attachments.len(),
        request
            .model_provider
            .as_ref()
            .map(|p| p.to_string())
            .unwrap_or_else(|| "None".to_string())
    );

    // 打印 agent_config 配置信息（debug 级别）
    info!(
        "🔧 [CHAT] agent_config: project_id={}, agent_config={:?}",
        project_id, request.agent_config
    );

    Ok(ChatRouteTarget {
        project_id,
        work_dir_id,
        container_work_path,
    })
}

/// 隔离类型参数校验：pod_id 存在时 isolation_type/tenant_id/space_id 必须非空，
/// 且 isolation_type 值有效（大小写不敏感）
#[allow(clippy::result_large_err)]
fn validate_chat_isolation_params(
    request: &AgentChatRequest,
    locale: &'static str,
) -> Result<(), ChatFlowExit> {
    if request.pod_id.is_none() {
        return Ok(());
    }

    if request.isolation_type.is_none() {
        error!("[CHAT] Validation failed: isolation_type is required when pod_id is provided");
        return Err(ChatFlowExit::Response(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "isolation_type is required when pod_id is provided",
        )));
    }
    if request.tenant_id.is_none() {
        error!("[CHAT] Validation failed: tenant_id is required when pod_id is provided");
        return Err(ChatFlowExit::Response(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "tenant_id is required when pod_id is provided",
        )));
    }
    if request.space_id.is_none() {
        error!("[CHAT] Validation failed: space_id is required when pod_id is provided");
        return Err(ChatFlowExit::Response(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "space_id is required when pod_id is provided",
        )));
    }

    // 验证 isolation_type 值有效（大小写不敏感）
    if let Some(ref it) = request.isolation_type
        && IsolationType::from_str(it).is_err()
    {
        error!(
            "[CHAT] Validation failed: invalid isolation_type '{}', expected tenant|space|project",
            it
        );
        return Err(ChatFlowExit::Response(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            &format!(
                "invalid isolation_type '{}', expected: tenant, space, project",
                it
            ),
        )));
    }

    // 记录验证通过的参数（此时 pod_id, isolation_type, tenant_id, space_id 必定为 Some）
    if let (Some(pid), Some(it), Some(tid), Some(sid)) = (
        request.pod_id.as_deref(),
        request.isolation_type.as_deref(),
        request.tenant_id.as_deref(),
        request.space_id.as_deref(),
    ) {
        info!(
            "🔒 [CHAT] Isolation parameters validated: pod_id={}, isolation_type={}, tenant_id={}, space_id={}",
            pid, it, tid, sid
        );
    }

    Ok(())
}

/// 第二段：容器目标解析
///
/// 第一步：获取或创建容器（默认使用 ServiceType::WebAgentRunner）；
/// 第二步：获取或创建 ProjectAndContainerInfo（使用存储）；
/// 随后立即更新活动时间，防止 gRPC 请求期间被 cleanup_task 误清理。
async fn resolve_container_target(
    state: &Arc<AppState>,
    request: &AgentChatRequest,
    project_id: &str,
    container_work_path: &str,
    service_type: &shared_types::ServiceType,
) -> Result<ContainerBasicInfo, AppError> {
    let container_options = service::container_manager::ContainerCreateOptions {
        project_id,
        service_type,
        request_resource_limits: resolve_resource_limits_from_config(
            state,
            service_type,
            request
                .agent_config
                .as_ref()
                .and_then(|c| c.resource_limits.clone()),
        )?,
        pod_id: request.pod_id.as_deref(),
        isolation_type: request.isolation_type.as_deref(),
        tenant_id: request.tenant_id.as_deref(),
        space_id: request.space_id.as_deref(),
        container_work_path,
        runtime: state.runtime(),
    };
    let container_info =
        service::container_manager::ContainerManager::get_or_create_container(container_options)
            .await?;

    // 第二步：获取或创建 ProjectAndContainerInfo - 使用 存储
    drop(ensure_project_record(
        state,
        request,
        project_id,
        &container_info,
        service_type,
    )?);

    // 请求到达时立即更新活动时间（不等待请求执行结果）
    // 这样可以防止在 gRPC 请求期间被 cleanup_task 误清理
    state.update_activity(project_id);
    debug!("[CHAT] Updated activity time: project_id={}", project_id);

    Ok(container_info)
}

/// 获取或创建 ProjectAndContainerInfo（存在则按需更新扩展状态，不存在则新建）
fn ensure_project_record(
    state: &Arc<AppState>,
    request: &AgentChatRequest,
    project_id: &str,
    container_info: &ContainerBasicInfo,
    service_type: &shared_types::ServiceType,
) -> Result<Arc<ProjectAndContainerInfo>, AppError> {
    info!("[CHAT] Getting/creating project: project_id={}", project_id);

    // 检查项目是否存在
    if let Some(existing_info) = state.get_project(project_id) {
        info!(
            "[CHAT] Project exists, checking for update: project_id={}",
            project_id
        );

        // 检查是否需要更新扩展状态
        // 重要：也需要检查 service_type 和 pod_id 是否需要更新
        // 如果 service_type 或 pod_id 不匹配，需要更新以确保 container_key() 返回正确的值
        let needs_extended_update = existing_info.container_info().is_none()
            || existing_info.model_provider().is_none()
            || existing_info.request_id().is_none()
            || existing_info.service_type() != Some(service_type.clone())
            || existing_info.pod_id() != request.pod_id.as_deref();

        if needs_extended_update {
            // 创建更新后的信息
            let mut mutable_info = (*existing_info).clone();
            // 补充 pod_id（兼容旧数据或服务重启后丢失的情况）
            if mutable_info.pod_id().is_none() && request.pod_id.is_some() {
                mutable_info.set_pod_id(request.pod_id.clone());
            }
            mutable_info.update_extended_from_request(
                Some(container_info.clone()),
                request.model_provider.clone(),
                request.request_id.clone(),
                Some(service_type.clone()),
            );
            mutable_info.update_activity();

            let arc_info = Arc::new(mutable_info);
            state
                .insert_project(project_id.to_string(), arc_info.clone())
                .map_err(|e| {
                    tracing::error!("[STORAGE] insert_project failed: {}", e);
                    e
                })?;

            info!(
                "✅ [CHAT] Project info fully updated: project_id={}, container_id={}",
                project_id, container_info.container_id
            );

            Ok(arc_info)
        } else {
            // 只需要更新活动时间
            state.update_activity(project_id);
            info!("[CHAT] Activity time updated: project_id={}", project_id);
            Ok(existing_info)
        }
    } else {
        info!("[CHAT] Creating new project: project_id={}", project_id);

        // 创建新的 ProjectAndContainerInfo
        let mut new_info = ProjectAndContainerInfo::new(project_id.to_string());
        new_info.set_pod_id(request.pod_id.clone());
        new_info.update_extended_from_request(
            Some(container_info.clone()),
            request.model_provider.clone(),
            request.request_id.clone(),
            Some(service_type.clone()),
        );
        new_info.set_scope(
            request.tenant_id.clone(),
            request.space_id.clone(),
            request.isolation_type.clone(),
        );

        let arc_info = Arc::new(new_info);
        state
            .insert_project(project_id.to_string(), arc_info.clone())
            .map_err(|e| {
                tracing::error!("[STORAGE] insert_project failed: {}", e);
                e
            })?;

        info!(
            "✅ [CHAT] Project info created: project_id={}, container_id={}",
            project_id, container_info.container_id
        );

        Ok(arc_info)
    }
}

/// 自动安装检查：如果 agent_server 携带 platforms，必须同时提供 agent_id、command、version
/// 内置 agent（容器预装）跳过安装逻辑
async fn ensure_chat_agent_installed_if_needed(
    state: &Arc<AppState>,
    request: &AgentChatRequest,
    project_id: &str,
    locale: &'static str,
    service_type: &shared_types::ServiceType,
) -> Result<(), ChatFlowExit> {
    let Some(ref agent_config) = request.agent_config else {
        return Ok(());
    };
    let Some(ref server) = agent_config.agent_server else {
        return Ok(());
    };
    let Some(ref platforms) = server.platforms else {
        return Ok(());
    };

    // agent_id 必填且非空
    let agent_id = match server.agent_id.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(id) => id,
        None => {
            error!("[CHAT] Validation failed: agent_id is required when platforms is provided");
            return Err(ChatFlowExit::Response(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                "agent_id is required and cannot be empty when platforms is provided",
            )));
        }
    };

    if !shared_types::is_builtin_agent(agent_id) {
        let command = match server.command.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(c) => c,
            None => {
                error!("[CHAT] Validation failed: command is required when platforms is provided");
                return Err(ChatFlowExit::Response(HttpResult::error_with_message(
                    shared_types::error_codes::ERR_VALIDATION,
                    locale,
                    "command is required and cannot be empty when platforms is provided",
                )));
            }
        };
        let version = match server.version.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(v) => v,
            None => {
                error!("[CHAT] Validation failed: version is required when platforms is provided");
                return Err(ChatFlowExit::Response(HttpResult::error_with_message(
                    shared_types::error_codes::ERR_VALIDATION,
                    locale,
                    "version is required and cannot be empty when platforms is provided",
                )));
            }
        };
        let args = server.args.as_deref().unwrap_or(&[]);

        info!(
            "📦 [CHAT] Auto-install: agent_id={}, version={}, args={:?}",
            agent_id, version, args
        );

        let install_req = handler::agent_install_strategy::AgentInstallRequest {
            agent_id,
            command,
            args,
            version,
            platforms,
        };
        handler::agent_install_strategy::ensure_agent_installed(
            state,
            project_id,
            &install_req,
            service_type,
        )
        .await?;
    } else {
        debug!(
            "📦 [CHAT] Builtin agent detected, skipping install: agent_id={}",
            agent_id
        );
    }

    Ok(())
}

/// 🆕 自动查找 session_id 逻辑
/// 如果用户没有传递 session_id，尝试从状态中查找最新的 session_id，
/// 并克隆 request 注入解析后的 session_id 用于转发。
fn resolve_chat_forward_request(
    state: &Arc<AppState>,
    request: &AgentChatRequest,
    project_id: &str,
) -> AgentChatRequest {
    let session_id_to_use = match &request.session_id {
        Some(sid) if !sid.is_empty() => {
            debug!("[CHAT] Using provided session_id: {}", sid);
            sid.clone()
        }
        _ => {
            // 用户没有传递 session_id，尝试查找最新的
            match state.get_project(project_id) {
                Some(project_info) => {
                    let existing_session_id = project_info.session_id();
                    match existing_session_id {
                        Some(sid) if !sid.is_empty() => {
                            info!(
                                "🔄 [CHAT] No session_id provided, auto using latest session: project_id={}, session_id={}",
                                project_id, sid
                            );
                            sid.to_string()
                        }
                        _ => {
                            debug!(
                                "[CHAT] No existing session_id for project, will create new session"
                            );
                            String::new()
                        }
                    }
                }
                None => {
                    debug!("[CHAT] No project exists, will create new session");
                    String::new()
                }
            }
        }
    };

    // 克隆 request 并修改 session_id
    let mut request_for_forward = request.clone();
    request_for_forward.session_id = if session_id_to_use.is_empty() {
        None
    } else {
        Some(session_id_to_use.clone())
    };
    // 🆕 自动查找 session_id 逻辑结束
    request_for_forward
}

/// 2.5 主动探测 agent_runner gRPC 是否就绪
/// K8s 下容器刚创建时 Pod 虽已 Ready，但 agent_runner 的 gRPC server 可能仍在启动，
/// 直接转发 Chat RPC 会 transport error。这里仿照 computer_chat_handler 做状态探活，
/// 并加重试以真正等待 gRPC server 就绪（正常情况下首次即成功，无额外延迟）。
async fn probe_agent_runner_readiness(
    state: &Arc<AppState>,
    container_info: &ContainerBasicInfo,
    project_id: &str,
    locale: &'static str,
) {
    // K8s 用 Service FQDN，Docker 用容器 IP（统一走 shared_types 分发）
    let grpc_addr = shared_types::build_grpc_addr(
        &container_info.container_name,
        &container_info.container_ip,
        &state.config.app_manager.namespace,
        &state.cluster_domain,
    );

    debug!(
        "[CHAT] Probing agent_runner readiness before forward: addr={}",
        grpc_addr
    );
    const MAX_PROBE_ATTEMPTS: u32 = 6;
    for attempt in 1..=MAX_PROBE_ATTEMPTS {
        let status_req = shared_types::grpc::GetStatusRequest {
            project_id: project_id.to_string(),
            session_id: String::new(),
        };
        let mut grpc_request = grpc::new_request_with_locale(status_req, locale);
        grpc_request.set_timeout(std::time::Duration::from_secs(3));

        let probed = match state.grpc_pool.get_client(&grpc_addr).await {
            Ok(mut client) => match client.get_status(grpc_request).await {
                Ok(resp) => {
                    debug!(
                        "📊 [CHAT] Agent ready: project_id={}, status={}, attempt={}",
                        project_id,
                        resp.into_inner().status,
                        attempt
                    );
                    true
                }
                Err(e) => {
                    warn!(
                        "⚠️ [CHAT] Agent status probe failed (attempt {}/{}): {}",
                        attempt, MAX_PROBE_ATTEMPTS, e
                    );
                    // 探活失败驱逐坏 channel，避免后续重试复用同一失效连接（对齐 forward 重试 remove）
                    state.grpc_pool.remove(&grpc_addr).await;
                    false
                }
            },
            Err(e) => {
                warn!(
                    "⚠️ [CHAT] Agent status probe get_client failed (attempt {}/{}): {}",
                    attempt, MAX_PROBE_ATTEMPTS, e
                );
                // 取不到/坏 channel 也驱逐，下次重试由连接池重建
                state.grpc_pool.remove(&grpc_addr).await;
                false
            }
        };

        if probed || attempt == MAX_PROBE_ATTEMPTS {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
    // 探活失败不阻止请求，保留原有降级行为（由后续 Chat RPC 自行处理）
}

/// 响应后状态更新 - 使用 存储
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
        },
    )
    .await;

    Ok(result)
}
