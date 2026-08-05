use super::helpers::*;
use super::*;
use crate::handler::utils::is_known_identifier;

/// 获取当前容器数量
///
/// 获取当前运行的容器总数及按服务类型分类的统计。
#[utoipa::path(
    get,
    path = "/computer/pod/count",
    responses(
        (status = 200, description = "成功获取容器数量", body = HttpResult<PodCountResponse>),
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_count",
    summary = "获取当前容器数量",
    description = "获取当前运行的容器总数及按服务类型分类的统计"
)]
pub async fn pod_count(
    State(state): State<Arc<AppState>>,
) -> Result<HttpResult<PodCountResponse>, AppError> {
    debug!(" [POD_COUNT] Getting container count");

    // 获取全局 Runtime
    let runtime = state.runtime().clone();

    // 获取所有容器列表
    let containers = runtime.list_containers().await.map_err(|e| {
        error!("[POD_COUNT] Failed to list containers: {}", e);
        AppError::internal_server_error(&format!("Failed to list containers: {}", e))
    })?;

    // 获取容器前缀（从 AppState 获取，启动时已初始化）
    let rcoder_prefix = state.container_prefix_rcoder.as_str();
    let computer_prefix = state.container_prefix_computer.as_str();

    // 按服务类型统计（仅统计运行中的容器）
    let mut rcoder_count = 0u32;
    let mut computer_count = 0u32;

    for container in &containers {
        // 仅统计运行中的容器
        if container.status != container_runtime_api::ContainerRuntimeStatus::Running {
            continue;
        }

        match container_identity_from_name(
            &container.container_name,
            rcoder_prefix,
            computer_prefix,
        )
        .map(|(_, service_type)| service_type)
        {
            Some(ServiceType::WebAgentRunner) => rcoder_count += 1,
            Some(ServiceType::ComputerAgentRunner) => computer_count += 1,
            // UserApp/UserAppBuilder 容器不计入 agent 统计
            Some(ServiceType::UserApp) | Some(ServiceType::UserAppBuilder) => {}
            None => {}
        }
    }

    let total_count = rcoder_count + computer_count;
    let timestamp = chrono::Utc::now().timestamp_millis().max(0) as u64;

    let response = PodCountResponse {
        total_count,
        by_service_type: PodCountByServiceType {
            rcoder: rcoder_count,
            computer_agent_runner: computer_count,
        },
        timestamp,
    };

    debug!(
        " [POD_COUNT] Container count completed: total={}, rcoder={}, computer_agent_runner={}",
        total_count, rcoder_count, computer_count
    );

    Ok(HttpResult::success(response))
}

/// 获取所有容器信息
///
/// 获取所有容器的详细信息，支持可选的分页查询（默认100条）。
/// 如果不传 limit 参数，则返回所有容器。
#[utoipa::path(
    get,
    path = "/computer/pod/list",
    params(
        PodListQuery
    ),
    responses(
        (status = 200, description = "成功获取容器列表", body = HttpResult<PodListResponse>),
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_list",
    summary = "获取所有容器信息",
    description = "获取所有容器的详细信息，支持可选的分页查询（默认100条）。如果不传 limit 参数，则返回所有容器。"
)]
#[instrument(skip(state))]
pub async fn pod_list(
    State(state): State<Arc<AppState>>,
    I18nQuery(params): I18nQuery<PodListQuery>,
) -> Result<HttpResult<PodListResponse>, AppError> {
    debug!(" [POD_LIST] get containers: limit={:?}", params.limit);

    // 1. 获取 runtime 容器列表
    let runtime = state.runtime().clone();

    let runtime_containers = runtime.list_containers().await.map_err(|e| {
        error!("[POD_LIST] Failed to list runtime containers: {}", e);
        AppError::internal_server_error(&format!("Failed to list runtime containers: {}", e))
    })?;

    // 2. 获取存储中的容器记录
    let stored_containers = state.projects.get_all_container_records();

    // 3. 获取容器前缀（从 AppState 获取，启动时已初始化）
    let rcoder_prefix = state.container_prefix_rcoder.as_str();
    let computer_prefix = state.container_prefix_computer.as_str();

    // 4. 创建容器ID到存储记录的映射
    let mut stored_map: std::collections::HashMap<String, &ContainerBasicInfo> =
        std::collections::HashMap::new();
    for record in &stored_containers {
        stored_map.insert(record.container_id.clone(), record);
    }

    // 5. 合并数据，构建容器详细信息列表
    let mut containers: Vec<PodDetailInfo> = Vec::new();

    for docker_container in &runtime_containers {
        // 仅处理运行中的容器
        if docker_container.status != container_runtime_api::ContainerRuntimeStatus::Running {
            continue;
        }

        let stored_record = stored_map.get(&docker_container.container_id);

        // 确定服务类型
        let container_identity = container_identity_from_name(
            &docker_container.container_name,
            rcoder_prefix,
            computer_prefix,
        );
        let service_type = container_identity
            .as_ref()
            .map(|(_, service_type)| service_type.to_string())
            .unwrap_or_else(|| "Unknown".to_string());

        // 从容器名称提取 user_id（如果是 computer-agent-runner-{user_id}）
        let user_id = match container_identity {
            Some((identifier, ServiceType::ComputerAgentRunner)) => Some(identifier.to_string()),
            _ => None,
        };

        // 获取项目ID和用户ID（从存储或Docker容器信息）
        let project_id = stored_record
            .and_then(|r| {
                // 尝试从存储关联的项目中获取project_id
                state
                    .projects
                    .get_projects_by_container_id(&r.container_id)
                    .first()
                    .map(|p| p.project_id().to_string())
            })
            .or_else(|| {
                // 如果存储中没有，使用Docker容器中的project_id
                if is_known_identifier(&docker_container.container_name) {
                    Some(docker_container.container_name.clone())
                } else {
                    None
                }
            });

        let final_user_id = user_id.or_else(|| {
            stored_record.and_then(|r| {
                state
                    .projects
                    .get_projects_by_container_id(&r.container_id)
                    .first()
                    .and_then(|p| p.user_id().map(|s| s.to_string()))
            })
        });

        // 构建容器详细信息
        let container_info = PodDetailInfo {
            container_id: docker_container.container_id.clone(),
            container_name: docker_container.container_name.clone(),
            container_ip: stored_record
                .map(|r| r.container_ip.clone())
                .unwrap_or_else(|| docker_container.container_ip.clone()),
            service_url: stored_record
                .map(|r| r.service_url.clone())
                .unwrap_or_else(|| {
                    format!(
                        "http://{}:{}",
                        docker_container.container_ip,
                        shared_types::HTTP_DEFAULT_PORT
                    )
                }),
            status: String::from(docker_container.status.clone()),
            service_type: service_type.to_string(),
            project_id,
            user_id: final_user_id,
            created_at: docker_container.created_at.timestamp_millis().max(0) as u64,
            last_activity: stored_record.map(|r| r.created_at.timestamp_millis().max(0) as u64),
            image: None,
            internal_port: stored_record.map(|r| r.internal_port),
            external_port: stored_record.map(|r| r.external_port),
        };

        containers.push(container_info);
    }

    // 5. 按创建时间倒序排序（最新的在前）
    containers.sort_by_key(|c| std::cmp::Reverse(c.created_at));

    // 6. 应用分页
    let total = containers.len() as u32;
    let limit = params.limit.unwrap_or(0);
    let paginated = limit > 0;
    let returned = if paginated {
        containers.truncate(limit as usize);
        limit.min(total)
    } else {
        total
    };

    let timestamp = chrono::Utc::now().timestamp_millis().max(0) as u64;

    let response = PodListResponse {
        containers,
        total,
        returned,
        paginated,
        timestamp,
    };

    info!(
        " [POD_LIST] Container list retrieved: total={}, returned={}, paginated={}",
        total, returned, paginated
    );

    Ok(HttpResult::success(response))
}

/// 容器保活（刷新活动时间）
///
/// 刷新容器的最后活动时间，防止被定时清理任务销毁。
/// 如果容器不存在会自动创建。
#[utoipa::path(
    post,
    path = "/computer/pod/keepalive",
    request_body(content = KeepalivePodRequest, description = "容器保活请求"),
    responses(
        (status = 200, description = "成功刷新活动时间", body = HttpResult<KeepalivePodResponse>),
        (status = 400, description = "请求参数无效", body = HttpResult<String>),
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_keepalive",
    summary = "容器保活（刷新活动时间）",
    description = "刷新容器的最后活动时间，防止被定时清理任务销毁。如果容器不存在会返回错误。"
)]
#[instrument(skip(state), fields(user_id = %request.user_id, project_id = %request.project_id))]
pub async fn pod_keepalive(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(request): I18nJsonOrQuery<KeepalivePodRequest>,
) -> Result<HttpResult<KeepalivePodResponse>, AppError> {
    let locale = shared_types::current_request_locale();

    // 1. 验证参数
    if request.user_id.trim().is_empty() {
        error!("[POD_KEEPALIVE] user_id is required");
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            "user_id is required and cannot be empty",
        ));
    }
    if request.project_id.trim().is_empty() {
        error!("[POD_KEEPALIVE] project_id is required");
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            "project_id is required and cannot be empty",
        ));
    }

    // 1.1 解析 service_type
    let service_type = match parse_service_type(request.service_type.as_deref()) {
        Ok(st) => st,
        Err(e) => {
            error!("[POD_KEEPALIVE] invalid service_type: {}", e);
            return Err(AppError::with_message(
                shared_types::error_codes::ERR_VALIDATION,
                e.to_string(),
            ));
        }
    };

    // 1.2 验证隔离参数完整性（当 pod_id 有值时）
    let container_identifier = if let Some(ref pod_id) = request.pod_id {
        if request.isolation_type.is_none()
            || request.tenant_id.is_none()
            || request.space_id.is_none()
        {
            error!(
                "[POD_KEEPALIVE] Validation failed: isolation_type, tenant_id, space_id are required when pod_id is provided"
            );
            return Err(AppError::with_message(
                shared_types::error_codes::ERR_VALIDATION,
                "isolation_type, tenant_id, space_id are all required when pod_id is provided",
            ));
        }
        // 记录验证通过的参数（此时 pod_id, isolation_type, tenant_id, space_id 必定为 Some）
        if let (Some(it), Some(tid), Some(sid)) = (
            request.isolation_type.as_deref(),
            request.tenant_id.as_deref(),
            request.space_id.as_deref(),
        ) {
            info!(
                " [POD_KEEPALIVE] Using pod_id for container lookup: pod_id={}, isolation_type={}, tenant_id={}, space_id={}",
                pod_id, it, tid, sid
            );
        }
        pod_id.clone()
    } else {
        // 根据 service_type 确定容器标识符
        container_identifier_for_service(
            &service_type,
            &request.user_id,
            &request.project_id,
            None,
        )?
    };

    info!(
        " [POD_KEEPALIVE] Container keepalive: user_id={}, project_id={}, container_identifier={}",
        request.user_id, request.project_id, container_identifier
    );

    // 2. 先确认容器存在（Docker 查询），不存在直接返回错误
    //
    // 修复（顺序问题）：必须先确认容器存在，再刷新活动时间。
    // 原实现先刷新 last_activity 再查 Docker，容器已被外部删除时会刷新僵尸记录的活动时间，
    // 且 storage 残留指向不存在容器的 project 记录。
    let container_info = match ComputerContainerManager::get_container_info_with_type(
        &container_identifier,
        state.runtime(),
        &service_type,
    )
    .await?
    {
        Some(info) => info,
        None => {
            info!(
                " [POD_KEEPALIVE] container not found: container_identifier={}",
                container_identifier
            );
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
                locale,
            ));
        }
    };

    // 3. 刷新活动时间（容器已确认存在）
    //
    // existed 语义：storage 中是否已有该 project 的记录。
    //   true  → 常规保活（update_activity 刷新 last_activity）
    //   false → 首次保活/容器恢复后首次（insert_project 新建记录，last_activity=now）
    //
    // created 与 existed 互逆：created=!existed 表示"本次是否新建了 storage 记录"。
    // 注意：keepalive 不创建容器（容器不存在直接返回错误），所以 created 不表示"容器是否新建"。
    let (previous_activity_time, current_activity_time, existed) = {
        if let Some(existing_info) = state.get_project(&request.project_id) {
            // storage 有记录：刷新当前 project 的 last_activity
            let prev = existing_info.last_activity().timestamp_millis().max(0) as u64;

            // 仅刷新当前 project 的 last_activity。
            // 共享容器（pod_id / user_id）的销毁判断由 cleanup_task 的 strategy 负责：
            // 只要容器关联的任一 project 活跃，容器就不会被销毁（见
            // computer_runner.rs 的 find_projects_by_user_id 和 rcoder.rs 的
            // find_projects_by_pod_id）。因此 keepalive 无需越权同步刷新其他 project。
            // 不活跃的 project 记录会被 cleanup 正常清理（但容器因活跃 project 保留）。
            let updated_time = state.update_activity(&request.project_id);
            let current = updated_time
                .map(|t| t.timestamp_millis().max(0) as u64)
                .unwrap_or_else(|| chrono::Utc::now().timestamp_millis().max(0) as u64);

            (prev, current, true)
        } else {
            // storage 无记录：容器已确认存在（Docker 查询通过），补建 storage 记录
            let mut project_info = ProjectAndContainerInfo::new(request.project_id.clone());
            project_info.set_user_id(Some(request.user_id.clone()));
            project_info.set_pod_id(request.pod_id.clone());
            project_info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
            project_info.set_scope(
                request.tenant_id.clone(),
                request.space_id.clone(),
                request.isolation_type.clone(),
            );
            project_info.set_container(Some(container_info.clone()));

            let now = chrono::Utc::now().timestamp_millis().max(0) as u64;

            state
                .insert_project(request.project_id.clone(), Arc::new(project_info))
                .map_err(|e| {
                    tracing::error!("[STORAGE] insert_project failed: {}", e);
                    e
                })?;
            info!(
                "[POD_KEEPALIVE] storage record created (container already exists): project_id={}",
                request.project_id
            );

            (0u64, now, false)
        }
    };

    // 4. 构建响应
    let created = !existed;
    let pod_container_info = PodContainerInfo {
        container_id: container_info.container_id.clone(),
        status: container_info.status.clone(),
    };

    // 从配置中获取清理超时时间
    let idle_timeout_seconds = state.config.cleanup_config.idle_timeout_seconds;

    let message = if created {
        // storage 记录首次创建（容器本身早已存在，只是 storage 没记录）
        format!(
            "Container record created, {} minutes until auto cleanup",
            idle_timeout_seconds / 60
        )
    } else {
        format!(
            "Container activity time refreshed, {} minutes until auto cleanup",
            idle_timeout_seconds / 60
        )
    };

    // 转换时间戳为东八区时间字符串
    let previous_activity_time_str = timestamp_to_utc8_string(previous_activity_time);
    let current_activity_time_str = timestamp_to_utc8_string(current_activity_time);

    let response = KeepalivePodResponse {
        existed: !created,
        created,
        container_info: pod_container_info,
        previous_activity_time,
        current_activity_time, // 使用实际数据库更新的时间
        previous_activity_time_str,
        current_activity_time_str,
        time_until_cleanup: idle_timeout_seconds,
        message,
    };

    info!(
        " [POD_KEEPALIVE] Keepalive completed: existed={}, created={}, time_until_cleanup={}s",
        !created, created, idle_timeout_seconds
    );

    Ok(HttpResult::success(response))
}

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
            .find_container(project_id, &shared_types::ServiceType::WebAgentRunner)
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
