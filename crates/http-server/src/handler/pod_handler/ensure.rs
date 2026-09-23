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
        (status = 200, description = "成功启动/获取容器；UserApp prod 操作占用返回 ERR_CONFLICT 信封，包含 blocker 及已知的 operation_id", body = HttpResult<EnsurePodResponse>),
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
        Ok(AppTarget::Prod(app_id)) => {
            return ensure_userapp_prod(&state, locale, app_id, request.user_id.as_str()).await;
        }
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
    _user_id: &str,
) -> Result<HttpResult<EnsurePodResponse>, AppError> {
    let (info, created) = crate::userapp_builder::ensure_userapp_builder_probed(state, &app_id)
        .await
        .map_err(|e| {
            error!("[POD_ENSURE] ensure userapp dev container failed: app_id={app_id}: {e:#}");
            crate::userapp_builder::control_error(&e)
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

/// ensure 的 userApp prod 分支：三态分派——已存在→唤醒（Ready/AlreadyRunning
/// 成功，Timeout/Failed 报错）；**不存在→空容器预创建**（复用 start 无 url
/// 三态链：supervisord 固定服务 PG/dbx/终端即可用、应用未部署，正式发布走
/// update 分支承接、PG 凭据由发布链 align）；API 查询故障→报错（
/// `fetch_runtime_status_or_err` 的精确分类保证只有"集群真不存在"才创建，
/// 瞬时故障不误建容器）。
async fn ensure_userapp_prod(
    state: &Arc<AppState>,
    locale: &str,
    app_id: String,
    _user_id: &str,
) -> Result<HttpResult<EnsurePodResponse>, AppError> {
    match state.app_service.get_app(&app_id).await {
        Ok(_) => {}
        Err(app_manager::AppOperationError::NotFound(_)) => {
            return ensure_userapp_prod_created(state, locale, app_id).await;
        }
        Err(e) => {
            // API Server 不可达/RBAC 拒绝等查询故障：语义=查询失败而非应用不存在，
            // 与唤醒路径的 Timeout/Failed 同码（Backend），不触发创建
            error!("[POD_ENSURE] query userapp prod app failed: app_id={app_id}: {e:#}");
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_BACKEND_ERROR,
                locale,
                &format!("query userapp prod app failed: {e:#}"),
            ));
        }
    }
    use shared_types::AppWakeControl;
    // 拍板 2026-09-23：手动 stop 与闲置回收统一——pod/ensure 与 rcoder-proxy
    // 被动流量共用 ensure_running 语义，有请求即唤醒。
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
        shared_types::WakeOutcome::Blocked { message, blocker } => {
            let mut response = HttpResult::error_with_message(
                shared_types::error_codes::ERR_CONFLICT,
                locale,
                &message,
            );
            if !blocker.operation_id.is_empty() {
                response = response.with_operation_id(blocker.operation_id.clone());
            }
            Ok(response.with_blocker(blocker))
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

/// prod 空容器预创建子分支：应用共享（无 owner 解析），复用 start 无 url
/// 三态链（不存在 → 创建空容器；deploy_controlled 的锁/幂等编排全程兜底，
/// 并发 ensure 由其 operation 锁收敛）。
async fn ensure_userapp_prod_created(
    state: &Arc<AppState>,
    locale: &str,
    app_id: String,
) -> Result<HttpResult<EnsurePodResponse>, AppError> {
    let request = app_manager::models::StartAppRequest::default();
    match state.app_service.start_app_enhanced(&app_id, request).await {
        Ok(result) => {
            info!(
                "[POD_ENSURE] userapp prod empty container created: app_id={app_id}, status={:?}, phase={:?}",
                result.runtime.status, result.runtime.phase
            );
            Ok(HttpResult::success(EnsurePodResponse {
                created: true,
                container_info: PodContainerInfo {
                    container_id: app_id.clone(),
                    status: format!("{:?}", result.runtime.status),
                },
                message: "Userapp 生产容器已预创建（应用未部署，PG/终端服务可用）".to_string(),
            }))
        }
        Err(e) => {
            error!(
                "[POD_ENSURE] create userapp prod empty container failed: app_id={app_id}: {e:#}"
            );
            Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_BACKEND_ERROR,
                locale,
                &format!("create userapp prod container failed: {e:#}"),
            ))
        }
    }
}
