//! pod 状态查询接口（status / vnc_status，从 queries.rs 拆出）。

use super::helpers::*;
use super::*;
use shared_types::ProjectStore as _; // 存储契约 trait：state.projects（ProjectStoreBackend）方法经此解析

/// 查询容器状态（是否存活）
///
/// 根据 user_id 或 project_id 查询对应容器是否存活。
/// 直接查询 Docker API 获取实时状态，无缓存延迟。
///
/// - 如果提供了 user_id，查询 `{container_prefix}-{user_id}` 容器
/// - 如果只提供 project_id，按 project_id 或容器名查询

#[utoipa::path(
    get,
    path = "/computer/pod/status",
    params(
        PodStatusQuery
    ),
    responses(
        (status = 200, description = "成功查询容器状态", body = HttpResult<PodStatusResponse>),
        (status = 400, description = "请求参数无效", body = HttpResult<String>),
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_status",
    summary = "查询容器状态（是否存活）",
    description = "根据 user_id 或 project_id 查询对应容器是否存活"
)]
#[instrument(skip(state), fields(project_id = ?params.project_id, user_id = ?params.user_id))]
pub async fn pod_status(
    State(state): State<Arc<AppState>>,
    I18nQuery(params): I18nQuery<PodStatusQuery>,
) -> Result<HttpResult<PodStatusResponse>, AppError> {
    let locale = shared_types::current_request_locale();

    // 1. 验证参数：至少需要 pod_id、user_id 或 project_id 之一
    if params.pod_id.is_none() && params.user_id.is_none() && params.project_id.is_none() {
        error!("[POD_STATUS] pod_id, user_id and project_id are all empty");
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            "at least one of pod_id, user_id or project_id is required",
        ));
    }

    // 1.1 解析 service_type
    let service_type = match parse_service_type(params.service_type.as_deref()) {
        Ok(st) => st,
        Err(e) => {
            error!("[POD_STATUS] invalid service_type: {}", e);
            return Err(AppError::with_message(
                shared_types::error_codes::ERR_VALIDATION,
                e.to_string(),
            ));
        }
    };

    // 1.2 验证隔离参数完整性（当 pod_id 有值时）
    let container_identifier = if let Some(ref pod_id) = params.pod_id {
        if params.isolation_type.is_none()
            || params.tenant_id.is_none()
            || params.space_id.is_none()
        {
            error!(
                "[POD_STATUS] Validation failed: isolation_type, tenant_id, space_id are required when pod_id is provided"
            );
            return Err(AppError::with_message(
                shared_types::error_codes::ERR_VALIDATION,
                "isolation_type, tenant_id, space_id are all required when pod_id is provided",
            ));
        }
        // 记录验证通过的参数（此时 pod_id, isolation_type, tenant_id, space_id 必定为 Some）
        if let (Some(it), Some(tid), Some(sid)) = (
            params.isolation_type.as_deref(),
            params.tenant_id.as_deref(),
            params.space_id.as_deref(),
        ) {
            info!(
                " [POD_STATUS] Using pod_id for container lookup: pod_id={}, isolation_type={}, tenant_id={}, space_id={}",
                pod_id, it, tid, sid
            );
        }
        Some(pod_id.clone())
    } else {
        None
    };

    info!(
        " [POD_STATUS] Querying container status: project_id={:?}, user_id={:?}, pod_id={:?}, container_identifier={:?}",
        params.project_id, params.user_id, params.pod_id, container_identifier
    );

    let timestamp = chrono::Utc::now().timestamp_millis().max(0) as u64;

    // 2. 获取 Runtime
    let runtime = state.runtime().clone();

    // 3. 查询容器状态
    // 优先级：pod_id > user_id > project_id
    let query_result = if let Some(ref identifier) = container_identifier {
        // 使用 pod_id 查找（多租户场景）
        runtime.find_container(identifier, &service_type).await
    } else if let Some(ref user_id) = params.user_id {
        runtime.find_container(user_id, &service_type).await
    } else if let Some(ref project_id) = params.project_id {
        runtime.find_container(project_id, &service_type).await
    } else {
        // 防御性编程：理论上不会到达这里（已在上方验证至少有一个标识符）
        // 但为了安全起见，返回验证错误而不是 panic
        error!("[POD_STATUS] Unexpected: all identifiers are None despite validation");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    };

    // 4. 通过 runtime 查询容器状态
    match query_result {
        Ok(Some(result)) => {
            let is_running =
                result.status == container_runtime_api::ContainerRuntimeStatus::Running;
            let status_str = if is_running { "running" } else { "stopped" };
            let message = if is_running {
                "container is running".to_string()
            } else {
                format!("container exists but status is: {:?}", result.status)
            };

            info!(
                " [POD_STATUS] Container status: alive={}, status={}, container_id={}",
                is_running, status_str, result.container_id
            );

            return Ok(HttpResult::success(PodStatusResponse {
                alive: is_running,
                status: status_str.to_string(),
                container_id: Some(result.container_id),
                container_name: Some(result.container_name),
                timestamp,
                message,
            }));
        }
        Ok(None) => {
            // 容器不存在，继续尝试 project_id
        }
        Err(e) => {
            error!("[POD_STATUS] Failed to query container status: {}", e);
            return Err(AppError::internal_server_error(&format!(
                "Failed to query container status: {}",
                e
            )));
        }
    }

    // 5. 如果用 user_id 没找到，且同时提供了 project_id，再试 project_id
    if params.user_id.is_some()
        && let Some(ref project_id) = params.project_id
    {
        match runtime
            .find_container(project_id, &ServiceType::WebAgentRunner)
            .await
        {
            Ok(Some(result)) => {
                let is_running =
                    result.status == container_runtime_api::ContainerRuntimeStatus::Running;
                let status_str = if is_running { "running" } else { "stopped" };
                let message = if is_running {
                    "container is running".to_string()
                } else {
                    format!("container exists but status is: {:?}", result.status)
                };

                info!(
                    " [POD_STATUS] Found container by project_id: alive={}, container_id={}",
                    is_running, result.container_id
                );

                return Ok(HttpResult::success(PodStatusResponse {
                    alive: is_running,
                    status: status_str.to_string(),
                    container_id: Some(result.container_id),
                    container_name: Some(result.container_name),
                    timestamp,
                    message,
                }));
            }
            Ok(None) => {
                // 容器不存在
            }
            Err(e) => {
                error!("[POD_STATUS] Query failed: {}", e);
                // 与第一路（user_id 查询）保持一致：runtime 错误返回 500，而非伪装成 not_found，
                // 否则客户端会误判"容器已销毁"并触发 ensure 重建风暴。
                return Err(AppError::internal_server_error(&format!(
                    "Failed to query container status: {}",
                    e
                )));
            }
        }
    }

    // 6. 未找到容器
    info!(
        " [POD_STATUS] Container not found: user_id={:?}, project_id={:?}",
        params.user_id, params.project_id
    );

    Ok(HttpResult::success(PodStatusResponse {
        alive: false,
        status: "not_found".to_string(),
        container_id: None,
        container_name: None,
        timestamp,
        message: format!(
            "Container not found (user_id={:?}, project_id={:?})",
            params.user_id, params.project_id
        ),
    }))
}

/// 查询容器 VNC 服务状态
///
/// 根据 user_id 或 project_id 定位容器，查询 VNC/noVNC 服务是否已启动就绪。
#[utoipa::path(
    get,
    path = "/computer/pod/vnc-status",
    params(VncStatusQuery),
    responses(
        (status = 200, description = "成功获取 VNC 状态", body = HttpResult<VncStatusResponse>),
        (status = 400, description = "参数无效", body = HttpResult<String>),
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
        (status = 404, description = "容器不存在", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_vnc_status",
    summary = "查询容器 VNC 服务状态",
    description = "根据 user_id 或 project_id 定位子容器，查询 VNC/noVNC 服务是否已启动就绪"
)]
#[instrument(skip(state))]
pub async fn pod_vnc_status(
    State(state): State<Arc<AppState>>,
    I18nQuery(params): I18nQuery<VncStatusQuery>,
) -> Result<HttpResult<VncStatusResponse>, AppError> {
    let locale = shared_types::current_request_locale();

    // 1. 参数验证：pod_id、user_id 和 project_id 不能同时为空
    let user_id = params.user_id.as_deref().filter(|s| !s.trim().is_empty());
    let project_id = params
        .project_id
        .as_deref()
        .filter(|s| !s.trim().is_empty());
    let pod_id = params.pod_id.as_deref().filter(|s| !s.trim().is_empty());

    // 1.1 解析 service_type
    let service_type = match parse_service_type(params.service_type.as_deref()) {
        Ok(st) => st,
        Err(e) => {
            error!("[POD_VNC_STATUS] invalid service_type: {}", e);
            return Err(AppError::with_message(
                shared_types::error_codes::ERR_VALIDATION,
                e.to_string(),
            ));
        }
    };

    // 1.2 验证隔离参数完整性（当 pod_id 有值时）
    if pod_id.is_some()
        && (params.isolation_type.is_none()
            || params.tenant_id.is_none()
            || params.space_id.is_none())
    {
        error!(
            "[POD_VNC_STATUS] Validation failed: isolation_type, tenant_id, space_id are required when pod_id is provided"
        );
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            "isolation_type, tenant_id, space_id are all required when pod_id is provided",
        ));
    }

    if pod_id.is_none() && user_id.is_none() && project_id.is_none() {
        warn!("[POD_VNC_STATUS] pod_id, user_id and project_id are all empty");
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            "at least one of pod_id, user_id or project_id is required",
        ));
    }

    info!(
        " [POD_VNC_STATUS] Querying VNC status: user_id={:?}, project_id={:?}, pod_id={:?}",
        user_id, project_id, pod_id
    );

    // 2. 获取 Runtime
    let runtime = state.runtime().clone();

    // 3. 定位容器
    // 优先级：pod_id > user_id > project_id
    let (_lookup_user_id, container_info) = if let Some(pid) = pod_id {
        // 使用 pod_id 查找（多租户场景）
        (pid, runtime.find_container(pid, &service_type).await)
    } else if let Some(uid) = user_id {
        (uid, runtime.find_container(uid, &service_type).await)
    } else if let Some(pid) = project_id {
        // 如果只有 project_id，通过 storage lookup 关联的容器
        if state
            .projects
            .get_container_by_user_id(pid, &service_type)
            .is_some()
        {
            // project_id 可能实际上是 user_id
            (pid, runtime.find_container(pid, &service_type).await)
        } else {
            (pid, Ok(None))
        }
    } else {
        ("", Ok(None))
    };

    let container_info = container_info.map_err(|e| {
        error!("[POD_VNC_STATUS] Failed to query container: {}", e);
        AppError::internal_server_error(&format!("Failed to query container: {}", e))
    })?;

    // 4. 检查容器是否存在
    let result = match container_info {
        Some(info) => info,
        None => {
            info!(
                " [POD_VNC_STATUS] Container does not exist: user_id={:?}, project_id={:?}",
                user_id, project_id
            );
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
                locale,
            ));
        }
    };

    // 5. 检查容器是否正在运行
    if result.status != container_runtime_api::ContainerRuntimeStatus::Running {
        info!(
            " [POD_VNC_STATUS] Container not running: container_id={}",
            result.container_id
        );
        return Ok(HttpResult::success(VncStatusResponse {
            vnc_ready: false,
            novnc_ready: false,
            message: "Container not running".to_string(),
            uptime_seconds: Some(0),
            container_id: Some(result.container_id),
        }));
    }

    // 5.1 🎯 确保 VNC 代理路由已注册
    // 解决竞态条件：VNC 服务已就绪，但代理路由尚未注册
    // 在 handle_computer_chat 时会注册路由，但 VNC 状态检查可能在 chat 之前调用
    if let Some(ref pingora_service) = state.pingora_service
        && let Some(uid) = user_id
    {
        pingora_service.add_vnc_backend(uid, &result.container_ip);
        debug!(
            "🔗 [POD_VNC_STATUS] Ensured VNC backend registered: user_id={} -> {}",
            uid, result.container_ip
        );
    }

    // 6. 构建 gRPC 地址
    // K8s 用 Service FQDN，Docker 用容器 IP（统一走 shared_types 分发）
    let grpc_addr = shared_types::build_grpc_addr(
        &result.container_name,
        &result.container_ip,
        &state.config.app_manager.namespace,
        &state.cluster_domain,
    );

    match state.grpc_pool.get_client(&grpc_addr).await {
        Ok(mut client) => {
            let grpc_request = crate::grpc::new_request_with_locale(
                shared_types::grpc::GetVncStatusRequest {
                    user_id: user_id.map(String::from),
                    project_id: project_id.map(String::from),
                },
                locale,
            );

            match client.get_vnc_status(grpc_request).await {
                Ok(response) => {
                    let resp = response.into_inner();
                    info!(
                        " [POD_VNC_STATUS] gRPC call successful: vnc_ready={}, novnc_ready={}",
                        resp.vnc_ready, resp.novnc_ready
                    );

                    Ok(HttpResult::success(VncStatusResponse {
                        vnc_ready: resp.vnc_ready,
                        novnc_ready: resp.novnc_ready,
                        message: resp.message,
                        uptime_seconds: Some(resp.uptime_seconds),
                        container_id: Some(result.container_id),
                    }))
                }
                Err(e) => {
                    error!("[POD_VNC_STATUS] gRPC call failed: {}", e);
                    Ok(HttpResult::error_with_locale(
                        shared_types::error_codes::ERR_GRPC_ERROR,
                        locale,
                    ))
                }
            }
        }
        Err(e) => {
            error!("[POD_VNC_STATUS] gRPC connection failed: {}", e);
            Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_GRPC_ERROR,
                locale,
            ))
        }
    }
}
