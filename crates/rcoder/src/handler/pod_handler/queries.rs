use super::*;
use crate::handler::utils::is_known_identifier;
use shared_types::ProjectStore as _; // 存储契约 trait：state.projects（ProjectStoreBackend）方法经此解析

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
    debug!("[POD_COUNT] Getting container count");

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
            // Userapp/UserappBuilder 容器不计入 agent 统计
            Some(ServiceType::Userapp) | Some(ServiceType::UserappBuilder) => {}
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
        "[POD_COUNT] Container count completed: total={}, rcoder={}, computer_agent_runner={}",
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
    debug!("[POD_LIST] get containers: limit={:?}", params.limit);

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
        "[POD_LIST] Container list retrieved: total={}, returned={}, paginated={}",
        total, returned, paginated
    );

    Ok(HttpResult::success(response))
}
