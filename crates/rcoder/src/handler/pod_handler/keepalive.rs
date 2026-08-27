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

    // 0. userApp 分派（app_id 存在即短路 agent 流程）
    match parse_app_target(
        request.app_id.as_deref(),
        request.app_stage.as_deref(),
        request.service_type.as_deref(),
    ) {
        Ok(AppTarget::NotApp) => {}
        Ok(AppTarget::Dev(app_id)) => {
            return keepalive_userapp_dev(&state, app_id, request.user_id.as_str()).await;
        }
        Ok(AppTarget::Prod(app_id)) => return keepalive_userapp_prod(&state, app_id).await,
        Err(e) => {
            error!("[POD_KEEPALIVE] invalid app target: {}", e);
            return Ok(invalid_app_target_response(locale, &e));
        }
    }

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
                "[POD_KEEPALIVE] Using pod_id for container lookup: pod_id={}, isolation_type={}, tenant_id={}, space_id={}",
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
        "[POD_KEEPALIVE] Container keepalive: user_id={}, project_id={}, container_identifier={}",
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
                "[POD_KEEPALIVE] container not found: container_identifier={}",
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
            // 记录解析出的真实类型（含 user-app/user-app-builder 场景）——下游
            // cleanup 策略选择与 adapter 索引门控依赖此标签；曾误硬编码
            // ComputerAgentRunner 导致补建的 userApp 记录走错回收策略
            project_info.set_service_type(Some(service_type.clone()));
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
        "[POD_KEEPALIVE] Keepalive completed: existed={}, created={}, time_until_cleanup={}s",
        !created, created, idle_timeout_seconds
    );

    Ok(HttpResult::success(response))
}

// ============================================================================
// userApp 分派实现（app_id/app_stage）
// ============================================================================

/// keepalive 的 userApp dev 分支：探活自愈（注册脏值重建——防死注册被周期
/// 保活永久续命）+ 刷新 last_activity 维持存活（此前 builder 活跃仅靠 chat）。
async fn keepalive_userapp_dev(
    state: &Arc<AppState>,
    app_id: String,
    user_id: &str,
) -> Result<HttpResult<KeepalivePodResponse>, AppError> {
    // 单次读取防两读间记录变动（previous 与 existed 自洽）
    let registered = state.get_project(&app_id);
    let previous = registered
        .as_ref()
        .map(|p| p.last_activity().timestamp_millis().max(0) as u64)
        .unwrap_or(0);
    let existed_before = registered
        .as_ref()
        .is_some_and(|p| p.container_info().is_some());
    drop(registered);

    let (info, created) =
        crate::userapp_builder::ensure_userapp_builder_probed(state, &app_id, Some(user_id))
            .await
            .map_err(|e| {
                error!(
                    "[POD_KEEPALIVE] ensure userapp dev container failed: app_id={app_id}: {e:#}"
                );
                AppError::with_message(
                    shared_types::error_codes::ERR_BACKEND_ERROR,
                    format!("ensure userapp dev container failed: {e:#}"),
                )
            })?;
    let current = state
        .update_activity(&app_id)
        .map(|t| t.timestamp_millis().max(0) as u64)
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis().max(0) as u64);
    let idle_timeout_seconds = state.config.cleanup_config.idle_timeout_seconds;
    info!(
        "[POD_KEEPALIVE] userapp dev container alive: app_id={app_id}, container={}",
        info.container_name
    );
    Ok(HttpResult::success(KeepalivePodResponse {
        existed: existed_before,
        created,
        container_info: PodContainerInfo {
            container_id: info.container_id.clone(),
            status: info.status.clone(),
        },
        previous_activity_time: previous,
        current_activity_time: current,
        previous_activity_time_str: timestamp_to_utc8_string(previous),
        current_activity_time_str: timestamp_to_utc8_string(current),
        time_until_cleanup: idle_timeout_seconds,
        message: format!(
            "UserApp dev 容器已保活, {} minutes until auto cleanup",
            idle_timeout_seconds / 60
        ),
    }))
}

/// keepalive 的 userApp prod 分支：AppAccessTracker.touch（生产回收信号源, 5s 节流）。
async fn keepalive_userapp_prod(
    state: &Arc<AppState>,
    app_id: String,
) -> Result<HttpResult<KeepalivePodResponse>, AppError> {
    use shared_types::AppAccessTracker;
    state.activity.touch(&app_id);
    // 真值时间戳（节流窗口内为上次 touch 时间——语义正确）
    let current = state
        .activity
        .last_accessed_at(&app_id)
        .map(|t| t.timestamp_millis().max(0) as u64)
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis().max(0) as u64);
    info!("[POD_KEEPALIVE] userapp prod app touched: app_id={app_id}");
    Ok(HttpResult::success(KeepalivePodResponse {
        existed: true,
        created: false,
        container_info: PodContainerInfo {
            container_id: app_id.clone(),
            status: "Running".to_string(),
        },
        previous_activity_time: current,
        current_activity_time: current,
        previous_activity_time_str: timestamp_to_utc8_string(current),
        current_activity_time_str: timestamp_to_utc8_string(current),
        // 生产回收阈值由 userapp_recycle 的 per-app 注解决定（非 cleanup_config）
        time_until_cleanup: 0,
        message: "UserApp 生产实例活跃信号已刷新（闲置回收计时重置）".to_string(),
    }))
}
