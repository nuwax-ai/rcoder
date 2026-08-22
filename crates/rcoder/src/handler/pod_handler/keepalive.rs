//! pod keepalive 接口（从 queries.rs 拆出）。

use super::helpers::*;
use super::*;

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
            project_info.set_service_type(Some(ServiceType::ComputerAgentRunner));
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
