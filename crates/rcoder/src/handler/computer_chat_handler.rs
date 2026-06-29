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

use axum::{extract::State, http::HeaderMap};
use shared_types::{ChatResponse, ComputerChatRequest, IsolationType};
use std::str::FromStr;
use std::sync::Arc;
use tracing::{debug, error, info, instrument, warn};

use crate::{AppError, HttpResult, router::AppState, service::ComputerContainerManager};
use docker_manager::ContainerBasicInfo;

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
    I18nJsonOrQuery(mut request): I18nJsonOrQuery<ComputerChatRequest>,
    is_devcomputer: bool,
) -> Result<HttpResult<ChatResponse>, AppError> {
    // 获取语言设置
    let locale = get_locale_from_headers(&headers);

    // 1. 验证 user_id
    if request.user_id.trim().is_empty() {
        error!("[COMPUTER_CHAT] user_id is required");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    }

    let user_id = request.user_id.clone();

    // ========== 隔离类型参数校验 ==========
    // IF pod_id IS NOT NULL THEN isolation_type, tenant_id, space_id 必须非空
    if request.pod_id.is_some() {
        if request.isolation_type.is_none() {
            error!(
                "[COMPUTER_CHAT] Validation failed: isolation_type is required when pod_id is provided"
            );
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                "isolation_type is required when pod_id is provided",
            ));
        }
        if request.tenant_id.is_none() {
            error!(
                "[COMPUTER_CHAT] Validation failed: tenant_id is required when pod_id is provided"
            );
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                "tenant_id is required when pod_id is provided",
            ));
        }
        if request.space_id.is_none() {
            error!(
                "[COMPUTER_CHAT] Validation failed: space_id is required when pod_id is provided"
            );
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                "space_id is required when pod_id is provided",
            ));
        }

        // 验证 isolation_type 值有效（大小写不敏感）
        if let Some(ref it) = request.isolation_type
            && IsolationType::from_str(it).is_err()
        {
            error!(
                "[COMPUTER_CHAT] Validation failed: invalid isolation_type '{}', expected tenant|space|project",
                it
            );
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                &format!(
                    "invalid isolation_type '{}', expected: tenant, space, project",
                    it
                ),
            ));
        }

        // 记录验证通过的参数（此时 pod_id, isolation_type, tenant_id, space_id 必定为 Some）
        if let (Some(pid), Some(it), Some(tid), Some(sid)) = (
            request.pod_id.as_deref(),
            request.isolation_type.as_deref(),
            request.tenant_id.as_deref(),
            request.space_id.as_deref(),
        ) {
            info!(
                "🔒 [COMPUTER_CHAT] Isolation parameters validated: pod_id={}, isolation_type={}, tenant_id={}, space_id={}",
                pid, it, tid, sid
            );
        }
    }

    // 2. 生成或使用提供的 project_id
    let project_id = match &request.project_id {
        Some(id) if !id.trim().is_empty() => id.clone(),
        _ => {
            let generated_id = crate::service::container_manager::generate_project_id();
            request.project_id = Some(generated_id.clone());
            generated_id
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
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            &e,
        ));
    }

    info!(
        "🚀 [COMPUTER_CHAT] Starting to process request: user_id={}, project_id={}, session_id={:?}, prompt_len={}, attachments={}, model_provider={:?}, agent_config={:?}",
        user_id,
        project_id,
        request.session_id,
        request.prompt.len(),
        request.attachments.len(),
        request.model_provider,
        request.agent_config
    );

    // 3. 验证资源限制配置
    if let Some(ref agent_config) = request.agent_config
        && let Some(ref resource_limits) = agent_config.resource_limits
        && let Err(e) = resource_limits.validate()
    {
        error!("[COMPUTER_CHAT] Resource limits validation failed: {}", e);
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_INVALID_RESOURCE_LIMITS,
            locale,
        ));
    }

    // 4. === 并发保护：检查是否有其他请求正在创建同一用户的容器 ===
    // 使用原子标记（DashMap）避免并发请求互相干扰，无死锁风险
    let mut waited_container_info: Option<ContainerBasicInfo> = None;

    // 🚀 关键修复：先订阅 broadcast channel，再检查 pod_creating
    // 避免 subscribe-after-send 竞态：如果在检查 pod_creating 之后才订阅，
    // 创建者可能已经移除了标记并发送了通知，导致我们错过消息。
    let mut rx = state.pod_created_tx.subscribe();

    // view() 在闭包返回后立即释放锁，无 Ref 暴露
    if let Some(elapsed) = state.pod_creating.view(&user_id, |_, t| t.elapsed()) {
        // 标记超过 60 秒视为过期（创建方可能已崩溃），忽略并继续
        if elapsed < std::time::Duration::from_secs(60) {
            info!(
                "⏳ [COMPUTER_CHAT] Container is being created, waiting for completion: user_id={}, elapsed={:?}",
                user_id, elapsed
            );

            match tokio::time::timeout(std::time::Duration::from_secs(30), async {
                loop {
                    match rx.recv().await {
                        Ok(created_user_id) if created_user_id == user_id => {
                            // 我们等待的容器已创建
                            break;
                        }
                        Ok(_) => continue, // 其他用户的容器，继续等待
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            // 通道关闭，退出
                            break;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // 消息丢失，检查标记是否已移除
                            if !state.pod_creating.contains_key(&user_id) {
                                break;
                            }
                            continue;
                        }
                    }
                }
            })
            .await
            {
                Ok(_) => {
                    // 容器创建成功，获取容器信息
                    if let Ok(Some(info)) = state
                        .runtime()
                        .get_container_info_by_identifier(
                            &user_id,
                            &shared_types::ServiceType::ComputerAgentRunner,
                        )
                        .await
                    {
                        info!(
                            "✅ [COMPUTER_CHAT] Wait successful, container is ready: user_id={}, container_id={}",
                            user_id, info.container_id
                        );
                        waited_container_info = Some(info);
                    }
                }
                Err(_) => {
                    // 超时处理
                    warn!(
                        "⚠️ [COMPUTER_CHAT] Wait for container creation timeout (30s), will try to create: user_id={}",
                        user_id
                    );
                }
            }
        } else {
            // 标记过期，清理后继续
            warn!(
                "⚠️ [COMPUTER_CHAT] Creation marker expired ({:?}), cleaning up and continuing",
                elapsed
            );
            state.pod_creating.remove(&user_id);
        }
    }

    // 5. 获取或创建用户容器
    let container_info = if let Some(info) = waited_container_info {
        // 使用等待获得的容器信息
        info!(
            "📦 [COMPUTER_CHAT] Using ready container (waiting for other request to finish creation): user_id={}, container_id={}",
            user_id, info.container_id
        );
        info
    } else {
        // 正常创建容器 - 设置标记防止并发
        state
            .pod_creating
            .insert(user_id.clone(), std::time::Instant::now());

        let options = crate::service::computer_container_manager::ContainerCreateOptions {
            user_id: user_id.clone(),
            project_id: project_id.clone(),
            resource_limits: request
                .agent_config
                .as_ref()
                .and_then(|c| c.resource_limits.clone()),
            pod_id: request.pod_id.clone(),
            isolation_type: request.isolation_type.clone(),
            tenant_id: request.tenant_id.clone(),
            space_id: request.space_id.clone(),
            service_type: shared_types::ServiceType::ComputerAgentRunner,
        };
        let result =
            ComputerContainerManager::get_or_create_container_for_user(&options, state.runtime())
                .await;

        // 清除标记（无论成功还是失败）
        state.pod_creating.remove(&user_id);

        // 🚀 发送容器创建完成通知（唤醒等待方）
        if result.is_ok() {
            let _ = state.pod_created_tx.send(user_id.clone());
        }

        match result {
            Ok(info) => info,
            Err(e) => {
                error!("[COMPUTER_CHAT] Failed to get or create container: {}", e);
                return Ok(HttpResult::error_with_locale(
                    shared_types::error_codes::ERR_CONTAINER_ERROR,
                    locale,
                ));
            }
        }
    };

    // 🛡️ 二次验证：确保容器 IP 非空
    // 容器管理器应该已经处理了空 IP 的情况，但缓存/Docker API 可能返回不一致结果
    // 如果 IP 为空，先清理旧容器再强制重建（不返回错误给客户端）
    let container_info = if container_info.container_ip.trim().is_empty() {
        warn!(
            "⚠️ [COMPUTER_CHAT] Container has empty IP after get_or_create, cleaning up and recreating: \
             user_id={}, old_container_id={}",
            user_id, container_info.container_id
        );
        // 必须先清理旧容器，否则 create_container 发现同名 "running" 容器会复用它
        let container_identifier = request.pod_id.as_deref().unwrap_or(&user_id);
        if let Err(e) = state
            .runtime()
            .stop_container_by_identifier(
                container_identifier,
                &shared_types::ServiceType::ComputerAgentRunner,
            )
            .await
        {
            warn!(
                "⚠️ [COMPUTER_CHAT] Failed to cleanup broken container before recreate: {}",
                e
            );
        }
        let options = crate::service::computer_container_manager::ContainerCreateOptions {
            user_id: user_id.clone(),
            project_id: user_id.clone(), // ComputerAgentRunner 使用 user_id 作为 project_id
            resource_limits: request
                .agent_config
                .as_ref()
                .and_then(|c| c.resource_limits.clone()),
            pod_id: request.pod_id.clone(),
            isolation_type: request.isolation_type.clone(),
            tenant_id: request.tenant_id.clone(),
            space_id: request.space_id.clone(),
            service_type: shared_types::ServiceType::ComputerAgentRunner,
        };
        ComputerContainerManager::force_create_container_for_user(&options, state.runtime())
            .await
            .map_err(|e| {
                error!("[COMPUTER_CHAT] Force recreate container failed: {}", e);
                AppError::with_message(
                    shared_types::error_codes::ERR_CONTAINER_ERROR,
                    format!("Container recreation failed: {}", e),
                )
            })?
    } else {
        container_info
    };

    debug!(
        "✅ [COMPUTER_CHAT] Container ready: user_id={}, container_id={}, ip={}",
        user_id, container_info.container_id, container_info.container_ip
    );

    // 🔍 检测 user_id 变化：同一个 project_id 被不同的 user_id 请求
    // 这通常意味着负载测试脚本使用了多个不同的 user_id，会导致创建多个容器浪费资源
    if let Some(existing_info) = state.get_project(&project_id)
        && let Some(existing_user_id) = existing_info.user_id()
        && existing_user_id != user_id
    {
        warn!(
            "⚠️ [USER_ID_MISMATCH] Detected user_id change for project_id: \
                     project_id={}, original user_id={}, new user_id={}, time={}. \
                     This may be caused by load test scripts using different user_ids, \
                     which creates multiple containers and wastes resources. \
                     Please ensure the same project_id uses the same user_id in your test scripts.",
            project_id,
            existing_user_id,
            user_id,
            chrono::Utc::now().to_rfc3339()
        );
    }

    // 🛡️ 关键修复：容器创建成功后立即插入 存储 记录
    // 这样可以防止孤立容器清理器误判并清理刚创建的容器
    //
    // 必须在 gRPC 请求之前就插入记录，因为：
    // 1. 孤立容器清理器会检查 存储 中是否存在该 user_id 的记录
    // 2. 如果记录不存在，容器会被判定为孤立并清理
    // 3. gRPC 请求是异步的，可能需要较长时间才能返回
    ensure_project_mapping_in_state(&state, &user_id, &project_id, &container_info, &request)?;

    // 请求到达时立即更新活动时间（不等待请求执行结果）
    // 这样可以防止在 gRPC 请求期间被 cleanup_task 误清理
    // 注意：这里使用 project_id 而不是 user_id，因为 存储 的 key 是 project_id
    state.update_activity(&project_id);
    debug!(
        "🔄 [COMPUTER_CHAT] Updated activity time: project_id={}",
        project_id
    );

    // 自动安装检查：如果 agent_server 携带 platforms，必须同时提供 agent_id、command、version
    // 内置 agent（容器预装）跳过安装逻辑
    if let Some(ref agent_config) = request.agent_config
        && let Some(ref server) = agent_config.agent_server
        && let Some(ref platforms) = server.platforms
    {
        // agent_id 必填且非空
        let agent_id = match server.agent_id.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(id) => id,
            None => {
                error!(
                    "[COMPUTER_CHAT] Validation failed: agent_id is required when platforms is provided"
                );
                return Ok(HttpResult::error_with_message(
                    shared_types::error_codes::ERR_VALIDATION,
                    locale,
                    "agent_id is required and cannot be empty when platforms is provided",
                ));
            }
        };

        if !shared_types::is_builtin_agent(agent_id) {
            let command = match server.command.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(c) => c,
                None => {
                    error!(
                        "[COMPUTER_CHAT] Validation failed: command is required when platforms is provided"
                    );
                    return Ok(HttpResult::error_with_message(
                        shared_types::error_codes::ERR_VALIDATION,
                        locale,
                        "command is required and cannot be empty when platforms is provided",
                    ));
                }
            };
            let version = match server.version.as_deref().filter(|s| !s.trim().is_empty()) {
                Some(v) => v,
                None => {
                    error!(
                        "[COMPUTER_CHAT] Validation failed: version is required when platforms is provided"
                    );
                    return Ok(HttpResult::error_with_message(
                        shared_types::error_codes::ERR_VALIDATION,
                        locale,
                        "version is required and cannot be empty when platforms is provided",
                    ));
                }
            };
            let args = server.args.as_deref().unwrap_or(&[]);

            info!(
                "📦 [COMPUTER_CHAT] Auto-install: agent_id={}, version={}, args={:?}",
                agent_id, version, args
            );

            let install_req = super::agent_install_strategy::AgentInstallRequest {
                agent_id,
                command,
                args,
                version,
                platforms,
            };
            super::agent_install_strategy::ensure_agent_installed(
                &state,
                &project_id,
                &install_req,
                &shared_types::ServiceType::ComputerAgentRunner,
            )
            .await?;
        } else {
            debug!(
                "📦 [COMPUTER_CHAT] Builtin agent detected, skipping install: agent_id={}",
                agent_id
            );
        }
    }

    // 5. 创建项目工作目录（在用户容器内）
    // Computer Agent Runner 需要在用户工作区内为 work_dir_id 创建子目录
    // 使用 ? 传播 AppError：验证错误 → HTTP 400，I/O 错误 → HTTP 500
    ensure_project_workspace_exists(
        request.isolation_type.as_deref(),
        request.tenant_id.as_deref(),
        request.space_id.as_deref(),
        &user_id,
        &work_dir_id,
    )
    .await?;

    // 6. 注册 VNC 后端到 Pingora（用于 WebSocket 代理）
    if let Some(ref pingora_service) = state.pingora_service {
        // 根据运行环境选择地址
        // - K8s 环境：使用 K8s Service FQDN
        // - Docker 环境：使用容器 IP
        let backend_addr = if shared_types::is_kubernetes_runtime() {
            super::utils::build_k8s_service_fqdn(
                &container_info.container_name,
                &state.config.app_manager.namespace,
                &state.cluster_domain,
            )
        } else {
            container_info.container_ip.clone()
        };
        pingora_service.add_vnc_backend(&user_id, &backend_addr);
        debug!(
            "🔗 [COMPUTER_CHAT] VNC backend registered: user_id={} -> {}",
            user_id, backend_addr
        );
    }

    // 6.5. 🆕 主动查询 Agent 状态 (User Request)
    // 在转发请求前，主动查询 Agent 状态，确保状态是最新的。
    // 这有助于在容器重启后，确认 Agent 是否真正处于空闲状态。
    {
        // 根据运行环境选择 gRPC 地址
        let grpc_addr_result = async {
            let addr = if shared_types::is_kubernetes_runtime() {
                let svc_fqdn = super::utils::build_k8s_service_fqdn(
                    &container_info.container_name,
                    &state.config.app_manager.namespace,
                    &state.cluster_domain,
                );
                format!("{}:{}", svc_fqdn, shared_types::GRPC_DEFAULT_PORT)
            } else {
                format!(
                    "{}:{}",
                    container_info.container_ip,
                    shared_types::GRPC_DEFAULT_PORT
                )
            };

            Ok::<_, String>(addr)
        }
        .await;

        if let Ok(grpc_addr) = grpc_addr_result {
            debug!("[COMPUTER_CHAT] Checking Agent status: {}", grpc_addr);
            if let Ok(mut client) = state.grpc_pool.get_client(&grpc_addr).await {
                let status_req = shared_types::grpc::GetStatusRequest {
                    project_id: project_id.clone(),
                    session_id: "".to_string(), // 我们只关心 project 级别的状态
                };

                let mut grpc_request = crate::grpc::new_request_with_locale(status_req, locale);
                grpc_request.set_timeout(std::time::Duration::from_secs(5));

                match client.get_status(grpc_request).await {
                    Ok(resp) => {
                        let status = resp.into_inner().status;
                        debug!(
                            "📊 [COMPUTER_CHAT] Agent current status: project_id={}, status={}",
                            project_id, status
                        );
                        // 如果状态是 idle，我们可以更有信心地继续
                    }
                    Err(e) => {
                        warn!("[COMPUTER_CHAT] Failed to get Agent status: {}", e);
                        // Query failed不阻止请求继续，可能是网络波动，让后续的 Chat 请求去处理
                    }
                }
            }
        }
    }

    // 7. 🆕 自动查找 session_id 逻辑
    // 如果用户没有传递 session_id，尝试从状态中查找最新的 session_id
    let session_id_to_use = match &request.session_id {
        Some(sid) if !sid.is_empty() => {
            debug!("[COMPUTER_CHAT] Using session_id: {}", sid);
            sid.clone()
        }
        _ => {
            // 用户没有传递 session_id，尝试查找最新的
            match state.get_project(&project_id) {
                Some(project_info) => {
                    let existing_session_id = project_info.session_id();
                    match existing_session_id {
                        Some(sid) if !sid.is_empty() => {
                            info!(
                                "🔄 [COMPUTER_CHAT] No session_id provided, auto using latest session: project_id={}, session_id={}",
                                project_id, sid
                            );
                            sid.to_string()
                        }
                        _ => {
                            debug!("[COMPUTER_CHAT] Project exists, creating new session");
                            String::new()
                        }
                    }
                }
                None => {
                    debug!("[COMPUTER_CHAT] No project, creating new session");
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

    // 8. 转发请求到容器服务（使用 gRPC）
    let forward_params = ComputerForwardParams {
        request: &request_for_forward,
        project_id: &project_id,
        work_dir_id: &work_dir_id,
        container_info: &container_info,
        grpc_pool: &state.grpc_pool,
        locale,
        is_devcomputer,
        namespace: &state.config.app_manager.namespace,
        cluster_domain: &state.cluster_domain,
    };
    let result = forward_computer_request_to_container(forward_params).await;

    // 8. 更新会话映射（填充所有三个映射表，保持一致性）
    // 无论请求成功还是失败，只要响应中包含 session_id，都要更新映射
    // 这样用户可以通过 SSE 接口获取错误通知，而不会收到 SESSION_EXPIRED 错误
    if let Some(chat_response) = &result.data {
        let session_id = chat_response.session_id.clone();

        // 只有当 session_id 非空时才更新映射
        if !session_id.is_empty() {
            info!(
                "🔗 [COMPUTER_CHAT] Associated session: session_id={} -> user_id={}, project_id={}, success={}",
                session_id,
                user_id,
                project_id,
                result.is_success()
            );

            // 从 Runtime API 获取最新容器信息，避免使用过期 IP
            let container_info = match state
                .runtime()
                .get_container_info_by_identifier(
                    &user_id,
                    &shared_types::ServiceType::ComputerAgentRunner,
                )
                .await
            {
                Ok(Some(info)) => {
                    info!(
                        "🔄 [COMPUTER_CHAT] Getting latest container info from Runtime API: user_id={}, container_id={}, container_ip={}",
                        user_id, info.container_id, info.container_ip
                    );
                    info
                }
                Ok(None) => {
                    warn!(
                        "⚠️ [COMPUTER_CHAT] Container not found in runtime: user_id={}, using cached container info",
                        user_id
                    );
                    // 使用之前获取的容器信息
                    container_info.clone()
                }
                Err(e) => {
                    warn!(
                        "⚠️ [COMPUTER_CHAT] Failed to get container info from runtime: user_id={}, error={}, using cached container info",
                        user_id, e
                    );
                    // 使用之前获取的容器信息
                    container_info.clone()
                }
            };

            // ComputerAgentRunner 模式：每个 project 独立记录
            // 使用真正的 project_id 作为 map_key，user_id 存储在数据字段中
            let map_key = project_id.clone();

            // 检查是否已存在该 project_id 的记录
            if let Some(existing_info) = state.get_project(&map_key) {
                // 已存在：更新信息
                let mut updated_info = (*existing_info).clone();

                // 更新活动时间
                updated_info.update_activity();
                // 添加 session（多 session 模型，不清除其他 session）
                updated_info.add_session(session_id.clone());

                // 更新扩展信息
                updated_info.update_extended_from_request(
                    Some(container_info.clone()),
                    request.model_provider.clone(),
                    request.request_id.clone(),
                    Some(shared_types::ServiceType::ComputerAgentRunner),
                );

                // 单次原子写入（项目元数据 + session 映射），消除 CAS 竞态
                state
                    .insert_project_with_session(
                        map_key.clone(),
                        Arc::new(updated_info),
                        &session_id,
                    )
                    .map_err(|e| {
                        tracing::error!("[STORAGE] insert_project_with_session failed: {}", e);
                        e
                    })?;

                info!(
                    "🔄 [COMPUTER_CHAT] Updated existing container mapping: user_id={}, project_id={}, session_id={} (last_activity refreshed)",
                    user_id, project_id, session_id
                );
            } else {
                // 不存在：创建新的 ProjectAndContainerInfo
                let mut project_info = shared_types::ProjectAndContainerInfo::new(map_key.clone());

                // 设置 user_id（ComputerAgentRunner 模式）
                project_info.set_user_id(Some(user_id.clone()));
                // 设置 pod_id（共享容器模式）
                project_info.set_pod_id(request.pod_id.clone());
                // 添加 session（多 session 模型）
                project_info.add_session(session_id.clone());

                // 更新扩展信息（容器、模型配置等）
                project_info.update_extended_from_request(
                    Some(container_info.clone()),
                    request.model_provider.clone(),
                    request.request_id.clone(),
                    Some(shared_types::ServiceType::ComputerAgentRunner),
                );

                // 单次原子写入（项目元数据 + session 映射），消除 CAS 竞态
                state
                    .insert_project_with_session(
                        map_key.clone(),
                        Arc::new(project_info),
                        &session_id,
                    )
                    .map_err(|e| {
                        tracing::error!("[STORAGE] insert_project_with_session failed: {}", e);
                        e
                    })?;

                info!(
                    "🆕 [COMPUTER_CHAT] Created new container mapping: user_id={}, project_id={}, session_id={}",
                    user_id, project_id, session_id
                );
            }

            if result.is_success() {
                info!(
                    "✅ [COMPUTER_CHAT] Request processed: user_id={}, project_id={}, session_id={} (all mappings updated)",
                    user_id, project_id, session_id
                );
            } else {
                warn!(
                    "⚠️ [COMPUTER_CHAT] Request failed but session mapping saved: user_id={}, project_id={}, session_id={}, code={}, message={}",
                    user_id, project_id, session_id, result.code, result.message
                );
            }
        }
    }

    if !result.is_success() && result.data.as_ref().is_none_or(|d| d.session_id.is_empty()) {
        error!(
            "❌ [COMPUTER_CHAT] Container service returned error (no session_id): user_id={}, project_id={}, code={}, message={}",
            user_id, project_id, result.code, result.message
        );
    }

    Ok(result)
}

/// 转发请求到容器内的 agent_runner 服务（仅使用 gRPC）
///
/// Computer Chat 转发请求参数
///
/// 封装了转发 Computer Chat 请求到容器服务所需的所有参数，
/// 避免函数参数过多，同时支持不同场景的扩展。
struct ComputerForwardParams<'a> {
    /// Computer Chat 请求
    request: &'a ComputerChatRequest,
    /// 项目 ID
    project_id: &'a str,
    /// 工作目录 ID
    work_dir_id: &'a str,
    /// 容器信息
    container_info: &'a ContainerBasicInfo,
    /// gRPC 连接池
    grpc_pool: &'a Arc<crate::grpc::GrpcChannelPool>,
    /// 语言设置
    locale: &'static str,
    /// 是否是 DevComputer 接口请求
    is_devcomputer: bool,
    /// K8s namespace
    namespace: &'a str,
    /// K8s 集群域名
    cluster_domain: &'a str,
}

/// 与 RCoder 的 forward_request_to_container_service 类似，
/// 但专门用于 ComputerAgentRunner 模式。
async fn forward_computer_request_to_container(
    params: ComputerForwardParams<'_>,
) -> HttpResult<ChatResponse> {
    info!(
        "📤 [COMPUTER_FORWARD] Forwarding request to container (gRPC): user_id={}, project_id={}, session_id={:?}, container_id={}, is_devcomputer={}",
        params.request.user_id,
        params.project_id,
        params.request.session_id,
        params.container_info.container_id,
        params.is_devcomputer
    );

    // 直接使用 gRPC 的健康检查机制，不额外检查容器状态
    // gRPC 连接失败会自动返回错误，由上层处理

    // 根据运行环境选择 gRPC 地址
    // - K8s 环境：使用 K8s Service FQDN（利用服务发现和负载均衡）
    // - Docker 环境：使用容器 IP（直接连接）
    let grpc_addr = if shared_types::is_kubernetes_runtime() {
        let addr = super::utils::build_k8s_grpc_addr(
            &params.container_info.container_name,
            params.namespace,
            params.cluster_domain,
        );
        debug!(
            "📡 [COMPUTER_FORWARD] Using K8s Service FQDN for gRPC: {}",
            addr
        );
        addr
    } else {
        let addr = format!(
            "{}:{}",
            params.container_info.container_ip,
            shared_types::GRPC_DEFAULT_PORT
        );
        debug!(
            "📡 [COMPUTER_FORWARD] Using container IP for gRPC: {}",
            addr
        );
        addr
    };

    debug!(
        "📡 [COMPUTER_FORWARD] gRPC address: {}, prompt_len={}, attachments={}",
        grpc_addr,
        params.request.prompt.len(),
        params.request.attachments.len()
    );

    // Computer Agent Runner 的工作目录路径
    // 在容器内：/app/computer-project-workspace/{user_id}/{work_dir_id}
    let project_workspace = match project_dir(&params.request.user_id, params.work_dir_id) {
        Ok(path) => format!("{}/", path),
        Err(e) => {
            return HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                params.locale,
                &e.to_string(),
            );
        }
    };

    debug!(
        "[COMPUTER_FORWARD] projectworkdirectory: {}",
        project_workspace
    );

    // gRPC 调用（带重试机制）
    let max_retries = 2;
    let mut last_error = None;

    for attempt in 1..=max_retries {
        let grpc_params = crate::grpc::GrpcChatParams {
            project_id: params.project_id.to_string(),
            session_id: params.request.session_id.clone(),
            prompt: params.request.prompt.clone(),
            attachments: params.request.attachments.clone(),
            data_source_attachments: params.request.data_source_attachments.clone(),
            model_config: params.request.model_provider.clone(),
            request_id: params.request.request_id.clone(),
            request_timeout: Some(std::time::Duration::from_secs(
                shared_types::GRPC_CHAT_TIMEOUT_SECS,
            )),
            system_prompt: params.request.system_prompt.clone(),
            user_prompt: params.request.user_prompt.clone(),
            agent_config: params.request.agent_config.clone(),
            service_type: Some(shared_types::ServiceType::ComputerAgentRunner),
            user_id: Some(params.request.user_id.clone()),
            is_devcomputer: params.is_devcomputer,
            agent_work_dir: params.request.agent_work_dir.clone(),
        };

        match crate::grpc::grpc_chat_with_pool(params.grpc_pool, &grpc_addr, grpc_params).await {
            Ok(grpc_response) => {
                if grpc_response.success {
                    let chat_response = crate::grpc::grpc_response_to_chat_response(grpc_response);
                    info!(
                        "✅ [COMPUTER_FORWARD] gRPC response success: project_id={}, session_id={}",
                        chat_response.project_id, chat_response.session_id
                    );
                    return HttpResult::success(chat_response);
                } else {
                    let error_msg = grpc_response
                        .error
                        .unwrap_or_else(|| "Unknown error".to_string());
                    // 🎯 从 gRPC 响应中提取错误码（完整透传）
                    let error_code = grpc_response
                        .error_code
                        .unwrap_or_else(|| shared_types::error_codes::ERR_AGENT_ERROR.to_string());
                    error!(
                        "❌ [COMPUTER_FORWARD] gRPC response error: code={}, message={}",
                        error_code, error_msg
                    );
                    return HttpResult::error(&error_code, &error_msg);
                }
            }
            Err(grpc_err) => {
                warn!(
                    "⚠️ [COMPUTER_FORWARD] gRPC call failed (attempt {}/{}): {}",
                    attempt, max_retries, grpc_err
                );

                // 使用 GrpcError 的 should_retry 方法，无需 downcast_ref
                let should_retry = grpc_err.should_retry();

                if should_retry && attempt < max_retries {
                    // 等待一段时间再重试，给 gRPC 服务启动时间
                    let retry_delay = std::time::Duration::from_secs(3);
                    info!(
                        "🔄 [COMPUTER_FORWARD] Detected retryable error, waiting {:?} before retry...",
                        retry_delay
                    );
                    tokio::time::sleep(retry_delay).await;

                    params.grpc_pool.remove(&grpc_addr).await;

                    // K8s Service FQDN 是稳定的，不需要重新解析
                    // 直接使用原来的 FQDN 进行重试
                    debug!(
                        "🔄 [COMPUTER_FORWARD] Retrying with same K8s Service FQDN: {}",
                        grpc_addr
                    );

                    last_error = Some(anyhow::Error::from(grpc_err));
                    continue;
                } else if !should_retry {
                    error!(
                        "[COMPUTER_FORWARD] Non-retryable error, stopped retry: {}",
                        grpc_err
                    );
                    last_error = Some(anyhow::Error::from(grpc_err));
                    break;
                }

                last_error = Some(anyhow::Error::from(grpc_err));
            }
        }
    }

    // 所有重试都失败
    if let Some(e) = last_error {
        error!(
            "❌ [COMPUTER_FORWARD] gRPC final call failed: {}, user_id={}, project_id={}",
            e, params.request.user_id, params.project_id
        );

        // gRPC 通信失败，直接返回错误
        // 注：业务错误码（如 Agent busy）现在由 agent_runner 通过 grpc_response.error_code 返回
        HttpResult::error_with_locale(shared_types::error_codes::ERR_GRPC_ERROR, params.locale)
    } else {
        HttpResult::error_with_locale(shared_types::error_codes::ERR_UNKNOWN, params.locale)
    }
}

/// 确保 project_id 对应的工作目录存在
///
/// Computer Agent Runner 的目录结构：
/// /app/computer-project-workspace/{user_id}/{project_id}/
///
/// 注意：这个目录已经在 docker-compose.yml 中挂载，可以直接在 rcoder 容器内创建
///
/// # 参数
/// - `isolation_type`: 隔离类型（可选）
/// - `tenant_id`: 租户 ID（可选）
/// - `space_id`: 空间 ID（可选）
/// - `user_id`: 用户 ID（当 isolation_type 为 project 时使用）
/// - `work_dir_id`: 工作目录标识符（可能是 project_id 或 agent_work_dir）
async fn ensure_project_workspace_exists(
    isolation_type: Option<&str>,
    tenant_id: Option<&str>,
    space_id: Option<&str>,
    user_id: &str,
    work_dir_id: &str,
) -> Result<(), AppError> {
    // 根据隔离类型构建工作空间路径
    let project_workspace_path = std::path::PathBuf::from(
        build_computer_workspace_path(isolation_type, tenant_id, space_id, user_id, work_dir_id)
            .map_err(|e| AppError::validation_error(&e.to_string()))?,
    );

    debug!(
        "📁 [COMPUTER_CHAT] Ensuring project workspace directory exists: {:?}",
        project_workspace_path
    );

    // 直接在 rcoder 容器内创建目录
    tokio::fs::create_dir_all(&project_workspace_path)
        .await
        .map_err(|e| {
            error!(
                "❌ [COMPUTER_CHAT] Failed to create project workspace: path={:?}, error={}",
                project_workspace_path, e
            );
            AppError::internal_server_error(&format!("Failed to create project workspace: {}", e))
        })?;

    debug!(
        "✅ [COMPUTER_CHAT] Project workspace directory created: user_id={}, work_dir_id={}, isolation_type={:?}, path={:?}",
        user_id, work_dir_id, isolation_type, project_workspace_path
    );

    Ok(())
}

/// 确保 存储 中存在 project_id 到容器的映射
///
/// 🛡️ 关键修复：容器创建成功后立即插入 存储 记录
///
/// 这样可以防止孤立容器清理器误判并清理刚创建的容器，因为：
/// 1. 孤立容器清理器会检查 存储 中是否存在该 user_id 关联的记录
/// 2. 如果记录不存在，容器会被判定为孤立并清理
/// 3. gRPC 请求是异步的，可能需要较长时间才能返回
///
/// # Arguments
/// * `state` - 应用状态
/// * `user_id` - 用户 ID
/// * `project_id` - 项目 ID
/// * `container_info` - 容器信息
/// * `request` - 聊天请求
fn ensure_project_mapping_in_state(
    state: &Arc<crate::router::AppState>,
    user_id: &str,
    project_id: &str,
    container_info: &ContainerBasicInfo,
    request: &ComputerChatRequest,
) -> Result<(), AppError> {
    // 检查是否已存在该 project_id 的记录
    let existing_project = state.get_project(project_id);
    if let Some(ref existing) = existing_project {
        // 如果记录存在，检查容器ID是否变更
        if let Some(existing_container) = existing.container_info() {
            if existing_container.container_id == container_info.container_id {
                debug!(
                    "🔄 [COMPUTER_CHAT] project record already exists and container unchanged: project_id={}",
                    project_id
                );
                return Ok(());
            } else {
                info!(
                    "🔄 [COMPUTER_CHAT] Detected container change: project_id={}, old_cid={}, new_cid={}",
                    project_id, existing_container.container_id, container_info.container_id
                );
                // 容器变更，继续执行后续的插入/更新逻辑（insert_project 会执行 upsert）
            }
        } else {
            // 现有记录没有容器信息，继续更新
        }
    }

    // 创建新的 ProjectAndContainerInfo
    let mut project_info = shared_types::ProjectAndContainerInfo::new(project_id.to_string());

    // 设置 user_id（ComputerAgentRunner 模式）
    project_info.set_user_id(Some(user_id.to_string()));
    // 设置 pod_id（共享容器模式）
    project_info.set_pod_id(request.pod_id.clone());

    // 🛡️ 关键修复：如果现有记录有 session_id，保留它
    // 多 session 模型下：把 existing 的所有 session 都迁过来（容器变更场景）
    if let Some(ref existing) = existing_project {
        let existing_sessions = existing.sessions();
        if !existing_sessions.is_empty() {
            for sid in &existing_sessions {
                project_info.add_session(sid.clone());
            }
            debug!(
                "🔄 [COMPUTER_CHAT] Preserved {} existing session(s): project_id={}",
                existing_sessions.len(),
                project_id
            );
        }
    }

    // 更新容器信息
    project_info.update_extended_from_request(
        Some(container_info.clone()),
        request.model_provider.clone(),
        request.request_id.clone(),
        Some(shared_types::ServiceType::ComputerAgentRunner),
    );

    // immediately insert project record
    // 注意：如果有现有 session，必须使用 insert_with_session 来同步更新 session_index
    // 否则容器重建后，session_index 中会丢失 session 映射，导致 SSE 连接失败
    let project_info_arc = Arc::new(project_info);
    let existing_sessions: Vec<String> = existing_project
        .as_ref()
        .map(|p| p.sessions().into_iter().collect())
        .unwrap_or_default();

    if existing_sessions.is_empty() {
        // 没有现有 session，直接插入
        state
            .insert_project(project_id.to_string(), project_info_arc)
            .map_err(|e| {
                tracing::error!("[STORAGE] insert_project failed: {}", e);
                e
            })?;
    } else {
        // 有现有 session，使用 insert_with_session 同步更新 session_index
        // 对于多个 session，先插入项目，再逐个添加 session
        state
            .insert_project(project_id.to_string(), project_info_arc.clone())
            .map_err(|e| {
                tracing::error!("[STORAGE] insert_project failed: {}", e);
                e
            })?;

        // 逐个添加现有 session 到 session_index
        for sid in &existing_sessions {
            state.add_session_to_project(project_id, sid);
        }

        debug!(
            "🔄 [COMPUTER_CHAT] Synced {} existing session(s) to session_index: project_id={}",
            existing_sessions.len(),
            project_id
        );
    }

    info!(
        "🆕 [COMPUTER_CHAT] Inserted project record (immediately after container creation): user_id={}, project_id={}, container_id={}",
        user_id, project_id, container_info.container_id
    );

    Ok(())
}

// ============================================================================
// SSE 进度流处理器（复用现有的 agent_session_notification）
// ============================================================================
