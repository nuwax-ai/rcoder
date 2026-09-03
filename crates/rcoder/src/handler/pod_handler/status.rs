//! pod 状态查询接口（从 queries.rs 拆出；vnc_status 已按端点轴拆至 vnc_status.rs）。

use super::helpers::*;
use super::*;

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
#[instrument(skip(state), fields(project_id = ?params.project_id, user_id = ?params.user_id, app_id = ?params.app_id))]
pub async fn pod_status(
    State(state): State<Arc<AppState>>,
    I18nQuery(params): I18nQuery<PodStatusQuery>,
) -> Result<HttpResult<PodStatusResponse>, AppError> {
    let locale = shared_types::current_request_locale();

    // 0. userApp 分派（app_id 存在即短路 agent 流程；service_type=userapp 搭配放行）
    match parse_app_target(
        params.app_id.as_deref(),
        params.app_stage.as_deref(),
        params.service_type.as_deref(),
    ) {
        Ok(AppTarget::NotApp) => {}
        Ok(AppTarget::Dev(app_id)) => return status_userapp_dev(&state, app_id).await,
        Ok(AppTarget::Prod(app_id)) => return status_userapp_prod(&state, app_id).await,
        Err(e) => {
            error!("[POD_STATUS] invalid app target: {}", e);
            return Err(AppError::with_message(
                shared_types::error_codes::ERR_VALIDATION,
                e,
            ));
        }
    }

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
                "[POD_STATUS] Using pod_id for container lookup: pod_id={}, isolation_type={}, tenant_id={}, space_id={}",
                pod_id, it, tid, sid
            );
        }
        Some(pod_id.clone())
    } else {
        None
    };

    info!(
        "[POD_STATUS] Querying container status: project_id={:?}, user_id={:?}, pod_id={:?}, container_identifier={:?}",
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
            let response = PodStatusResponse::from_runtime(&result, timestamp);
            info!(
                "[POD_STATUS] Container status: alive={}, status={}, container_id={}",
                response.alive, response.status, result.container_id
            );
            return Ok(HttpResult::success(response));
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
                let response = PodStatusResponse::from_runtime(&result, timestamp);
                info!(
                    "[POD_STATUS] Found container by project_id: alive={}, container_id={}",
                    response.alive, result.container_id
                );
                return Ok(HttpResult::success(response));
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
        "[POD_STATUS] Container not found: user_id={:?}, project_id={:?}",
        params.user_id, params.project_id
    );

    Ok(HttpResult::success(PodStatusResponse::not_found(
        timestamp,
        format!(
            "Container not found (user_id={:?}, project_id={:?})",
            params.user_id, params.project_id
        ),
    )))
}

impl PodStatusResponse {
    /// runtime 命中组装（user_id 主路 / project_id 回退路共享）：
    /// 枚举 status 推导 alive/status/message 三元组
    fn from_runtime(result: &container_runtime_api::RuntimeContainerInfo, timestamp: u64) -> Self {
        let is_running = result.status == container_runtime_api::ContainerRuntimeStatus::Running;
        Self {
            alive: is_running,
            status: if is_running { "running" } else { "stopped" }.to_string(),
            container_id: Some(result.container_id.clone()),
            container_name: Some(result.container_name.clone()),
            timestamp,
            message: if is_running {
                "container is running".to_string()
            } else {
                format!("container exists but status is: {:?}", result.status)
            },
        }
    }

    /// not_found 组装（agent 主路径 / userapp dev / userapp prod 三路共享，
    /// message 由调用方携带上下文）
    fn not_found(timestamp: u64, message: String) -> Self {
        Self {
            alive: false,
            status: "not_found".to_string(),
            container_id: None,
            container_name: None,
            timestamp,
            message,
        }
    }
}

// ============================================================================
// userApp 分派实现（app_id/app_stage/service_type=userapp）
// ============================================================================

/// status 的 userApp dev 分支：查询 UserappBuilder 开发容器实时状态（只读，
/// 不触发探活自愈——那是 ensure/keepalive 的职责）。
async fn status_userapp_dev(
    state: &Arc<AppState>,
    app_id: String,
) -> Result<HttpResult<PodStatusResponse>, AppError> {
    let timestamp = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let existing = state
        .runtime()
        .get_container_info_by_identifier(&app_id, &ServiceType::UserappBuilder)
        .await
        .map_err(|e| {
            error!("[POD_STATUS] userapp dev container lookup failed: app_id={app_id}: {e:#}");
            AppError::internal_server_error(&format!("userapp dev container lookup failed: {e:#}"))
        })?;
    let Some(info) = existing else {
        info!("[POD_STATUS] userapp dev container not found: app_id={app_id}");
        return Ok(HttpResult::success(PodStatusResponse::not_found(
            timestamp,
            format!("Userapp dev container not found (app_id={app_id})"),
        )));
    };
    // ContainerBasicInfo.status 是运行时自由字符串（"Running"/"Starting"/pod phase），
    // 大小写容忍比较（K8s Pod phase 为 "Running"）。
    let is_running = is_container_running(&info.status);
    info!(
        "[POD_STATUS] userapp dev container status: app_id={app_id}, alive={}, container_id={}",
        is_running, info.container_id
    );
    Ok(HttpResult::success(PodStatusResponse {
        alive: is_running,
        status: if is_running { "running" } else { "stopped" }.to_string(),
        container_id: Some(info.container_id),
        container_name: Some(info.container_name),
        timestamp,
        message: if is_running {
            "Userapp dev container is running".to_string()
        } else {
            format!(
                "Userapp dev container exists but status is: {:?}",
                info.status
            )
        },
    }))
}

/// status 的 userApp prod 分支：alive 按探针口径（`ready_replicas > 0`——prod
/// Deployment 创建时默认配 readinessProbe 打 app-cli `/ready`，ready_replicas 即
/// "探针通过副本数"）。app 不存在→not_found（200）；集群 API 故障→500，不伪装成
/// not_found（防客户端误判容器已销毁触发 ensure 重建风暴，与 agent 路径同款语义）。
async fn status_userapp_prod(
    state: &Arc<AppState>,
    app_id: String,
) -> Result<HttpResult<PodStatusResponse>, AppError> {
    let timestamp = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let runtime_info = match state.app_service.get_app(&app_id).await {
        Ok(info) => info,
        Err(app_manager::AppOperationError::NotFound(_)) => {
            info!("[POD_STATUS] userapp prod app not found: app_id={app_id}");
            return Ok(HttpResult::success(PodStatusResponse::not_found(
                timestamp,
                format!("Userapp prod app not found (app_id={app_id})"),
            )));
        }
        Err(e) => {
            error!("[POD_STATUS] userapp prod app query failed: app_id={app_id}: {e:#}");
            return Err(AppError::internal_server_error(&format!(
                "userapp prod app query failed: {e:#}"
            )));
        }
    };
    let alive = runtime_info.ready_replicas > 0;
    info!(
        "[POD_STATUS] userapp prod app status: app_id={app_id}, alive={}, phase={}, replicas={}/{} ready",
        alive, runtime_info.phase, runtime_info.ready_replicas, runtime_info.replicas
    );
    Ok(HttpResult::success(PodStatusResponse {
        alive,
        status: runtime_info.phase.to_ascii_lowercase(),
        container_id: Some(app_id),
        container_name: None,
        timestamp,
        message: format!(
            "phase={}, health={}, replicas={}/{} ready",
            runtime_info.phase,
            runtime_info.health.status,
            runtime_info.ready_replicas,
            runtime_info.replicas
        ),
    }))
}
