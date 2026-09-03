use super::ensure_flow;
use super::helpers::*;
use super::*;

/// 启动/确保容器存在（幂等）
///
/// 根据 user_id 和 project_id 启动或获取已存在的容器。
/// 仅启动容器，不启动 Agent 服务。
#[utoipa::path(
    post,
    path = "/computer/pod/ensure",
    request_body(content = EnsurePodRequest, description = "启动容器请求"),
    responses(
        (status = 200, description = "成功启动/获取容器", body = HttpResult<EnsurePodResponse>),
        (status = 400, description = "请求参数无效", body = HttpResult<String>),
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_ensure",
    summary = "启动/确保容器存在（幂等）",
    description = "根据 user_id 和 project_id 启动或获取已存在的容器，仅启动容器不启动 Agent 服务"
)]
#[instrument(skip(state), fields(user_id = %request.user_id, project_id = %request.project_id))]
pub async fn pod_ensure(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(request): I18nJsonOrQuery<EnsurePodRequest>,
) -> Result<HttpResult<EnsurePodResponse>, AppError> {
    let locale = shared_types::current_request_locale();

    // 0. userApp 分派（app_id 存在即短路 agent 流程）
    match parse_app_target(
        request.app_id.as_deref(),
        request.app_stage.as_deref(),
        request.service_type.as_deref(),
    ) {
        Ok(AppTarget::NotApp) => {}
        Ok(AppTarget::Dev(app_id)) => {
            return ensure_userapp_dev(&state, app_id, request.user_id.as_str()).await;
        }
        Ok(AppTarget::Prod(app_id)) => return ensure_userapp_prod(&state, locale, app_id).await,
        Err(e) => {
            error!("[POD_ENSURE] invalid app target: {}", e);
            return Ok(invalid_app_target_response(locale, &e));
        }
    }

    // 1. 验证参数
    if let Some(resp) =
        validate_pod_ids(&request.user_id, &request.project_id, locale, "POD_ENSURE")
    {
        return Ok(resp);
    }

    // 1.1 验证资源限制
    if let Some(ref limits) = request.resource_limits
        && let Err(e) = validate_resource_limits(limits)
    {
        error!("[POD_ENSURE] resources update failed: {}", e);
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_INVALID_RESOURCE_LIMITS,
            locale,
            &e,
        ));
    }

    // 1.2 解析 service_type
    let service_type = match parse_service_type(request.service_type.as_deref()) {
        Ok(st) => st,
        Err(e) => {
            error!("[POD_ENSURE] invalid service_type: {}", e);
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                &e,
            ));
        }
    };

    // 1.3 根据 service_type 确定容器标识符
    let container_identifier = container_identifier_for_service(
        &service_type,
        &request.user_id,
        &request.project_id,
        request.pod_id.as_deref(),
    )?;

    info!(
        "[POD_ENSURE] Ensuring container exists: user_id={}, project_id={}, service_type={}, container_identifier={}",
        request.user_id, request.project_id, service_type, container_identifier
    );

    // === 并发保护：检查是否有其他请求正在创建同一用户的容器 ===
    // 使用原子标记（DashMap）避免并发请求互相干扰，无死锁风险
    if let Some(response) = ensure_flow::wait_for_concurrent_creation(
        &state,
        &request,
        &service_type,
        &container_identifier,
    )
    .await
    {
        return response;
    }

    // 2. 🔍 实时查询 runtime 检查容器是否存在（不依赖缓存），未运行的旧容器
    // 连同 SSE/gRPC 连接一并清理
    let need_create =
        ensure_flow::resolve_need_create(&state, &container_identifier, &service_type).await?;

    // 3. 获取或创建容器（带重试机制 + 标记）
    let (container_info, created) = if need_create {
        let info =
            ensure_flow::create_with_retry(&state, &request, &service_type, &container_identifier)
                .await?;
        (info, true)
    } else {
        ensure_flow::get_existing_with_sync(&state, &request, &service_type, &container_identifier)
            .await?
    };

    // 4/5/6. 注册 VNC backend + 更新存储记录 + 构建响应
    let message = if created {
        "Container created successfully, can access virtual desktop via VNC (Agent service not started)".to_string()
    } else {
        "Container already exists, can access virtual desktop via VNC directly".to_string()
    };
    persist_and_respond(
        &state,
        &request,
        &service_type,
        &container_info,
        created,
        message,
    )
}

// ============================================================================
// userApp 分派实现（app_id/app_stage）
// ============================================================================

/// ensure 的 userApp dev 分支：探活自愈版 ensure（注册脏值/死容器重建），
/// created 由探活版判定（复用=false / 重建或新建=true）。
async fn ensure_userapp_dev(
    state: &Arc<AppState>,
    app_id: String,
    user_id: &str,
) -> Result<HttpResult<EnsurePodResponse>, AppError> {
    let (info, created) =
        crate::userapp_builder::ensure_userapp_builder_probed(state, &app_id, Some(user_id))
            .await
            .map_err(|e| {
                error!("[POD_ENSURE] ensure userapp dev container failed: app_id={app_id}: {e:#}");
                AppError::with_message(
                    shared_types::error_codes::ERR_BACKEND_ERROR,
                    format!("ensure userapp dev container failed: {e:#}"),
                )
            })?;
    info!(
        "[POD_ENSURE] userapp dev container ready: app_id={app_id}, container={}, ip={}",
        info.container_name, info.container_ip
    );
    Ok(HttpResult::success(EnsurePodResponse {
        created,
        container_info: PodContainerInfo {
            container_id: info.container_id.clone(),
            status: info.status.clone(),
        },
        message: "Userapp dev 容器已就绪（虚拟终端/文件服务经反向代理访问）".to_string(),
    }))
}

/// ensure 的 userApp prod 分支：先验存在（防对不存在 app_id 幻报 AlreadyRunning），
/// 再唤醒（Ready/AlreadyRunning 成功，Timeout/Failed 报错）。
async fn ensure_userapp_prod(
    state: &Arc<AppState>,
    locale: &str,
    app_id: String,
) -> Result<HttpResult<EnsurePodResponse>, AppError> {
    // 存在性校验：ensure_running 只在 stopped 集合命中时走唤醒，不存在的 app
    // 会返回 AlreadyRunning——必须先经 get_app 拦住幻报
    if let Err(e) = state.app_service.get_app(&app_id).await {
        error!("[POD_ENSURE] userapp prod app not found: app_id={app_id}: {e:#}");
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
            locale,
            &format!("userapp prod app not found: {e:#}"),
        ));
    }
    use shared_types::AppWakeControl;
    let outcome = state.activity.ensure_running(&app_id).await;
    match outcome {
        shared_types::WakeOutcome::Ready => Ok(HttpResult::success(EnsurePodResponse {
            created: true,
            container_info: PodContainerInfo {
                container_id: app_id.clone(),
                status: "Running".to_string(),
            },
            message: "Userapp 生产实例已唤醒（wake_on_traffic 已启用）".to_string(),
        })),
        shared_types::WakeOutcome::AlreadyRunning => Ok(HttpResult::success(EnsurePodResponse {
            created: false,
            container_info: PodContainerInfo {
                container_id: app_id.clone(),
                status: "Running".to_string(),
            },
            message: "Userapp 生产实例已在运行".to_string(),
        })),
        shared_types::WakeOutcome::Timeout => {
            error!("[POD_ENSURE] userapp prod wake timeout: app_id={app_id}");
            Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_BACKEND_ERROR,
                locale,
                "userapp prod ensure failed: wake timeout",
            ))
        }
        shared_types::WakeOutcome::Failed(e) => {
            error!("[POD_ENSURE] userapp prod wake failed: app_id={app_id}: {e}");
            Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_BACKEND_ERROR,
                locale,
                &format!("userapp prod ensure failed: {e}"),
            ))
        }
    }
}
