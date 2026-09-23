use super::ensure_flow;
use super::helpers::*;
use super::*;
use axum::response::IntoResponse;

/// 启动/确保容器存在（幂等）
///
/// 根据 user_id 和 project_id 启动或获取已存在的容器。
/// 仅启动容器，不启动 Agent 服务。
#[utoipa::path(
    post,
    path = "/computer/pod/ensure",
    request_body(content = EnsurePodRequest, description = "启动容器请求"),
    responses(
        (status = 200, description = "容器已存在或普通容器已同步创建；UserApp 只表示计算资源已请求运行，不代表业务 Ready", body = HttpResult<EnsurePodResponse>),
        (status = 202, description = "已有 UserApp 计算资源需要物理启动；返回持久操作 ID 和查询地址，不等待业务服务 Ready", body = HttpResult<crate::userapp_builder::compute_control::ComputeOperationView>),
        (status = 400, description = "请求参数无效", body = HttpResult<String>),
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_ensure",
    summary = "启动/确保容器存在（幂等）",
    description = "根据 user_id 和 project_id 启动或获取已存在的容器，仅启动容器不启动 Agent 服务。UserApp 已有控制器但计算资源停止时走高优先级物理控制操作，Kubernetes 启动时将 Pod 模板更新为当前平台镜像并保留 PVC；HTTP 202 返回 operation_id，业务 Ready 单独查询。已运行的容器直接返回，不因 ensure 升级镜像。Stop/Restart 正在执行时快失败，不内部排队。"
)]
#[instrument(skip(state), fields(user_id = %request.user_id, project_id = %request.project_id))]
pub async fn pod_ensure(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(request): I18nJsonOrQuery<EnsurePodRequest>,
) -> Result<axum::response::Response, AppError> {
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
            return Ok(invalid_app_target_response::<EnsurePodResponse>(locale, &e).into_response());
        }
    }

    ensure_ordinary_pod(&state, &request, locale)
        .await
        .map(IntoResponse::into_response)
}

pub(crate) async fn ensure_ordinary_pod(
    state: &Arc<AppState>,
    request: &EnsurePodRequest,
    locale: &'static str,
) -> Result<HttpResult<EnsurePodResponse>, AppError> {
    // 1. 验证参数
    if let Some(resp) = validate_pod_ids::<EnsurePodResponse>(
        &request.user_id,
        &request.project_id,
        locale,
        "POD_ENSURE",
    ) {
        return Ok(resp);
    }

    // 1.1 验证资源限制
    if let Some(ref limits) = request.resource_limits
        && let Err(e) = validate_resource_limits(limits)
    {
        error!("[POD_ENSURE] resources update failed: {}", e);
        return Ok(HttpResult::<EnsurePodResponse>::error_with_message(
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
            return Ok(HttpResult::<EnsurePodResponse>::error_with_message(
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
        state,
        request,
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
        ensure_flow::resolve_need_create(state, &container_identifier, &service_type).await?;

    // 3. 获取或创建容器（带重试机制 + 标记）
    let (container_info, created) = if need_create {
        let info =
            ensure_flow::create_with_retry(state, request, &service_type, &container_identifier)
                .await?;
        (info, true)
    } else {
        ensure_flow::get_existing_with_sync(state, request, &service_type, &container_identifier)
            .await?
    };

    // 4/5/6. 注册 VNC backend + 更新存储记录 + 构建响应
    let message = if created {
        "Container created successfully, can access virtual desktop via VNC (Agent service not started)".to_string()
    } else {
        "Container already exists, can access virtual desktop via VNC directly".to_string()
    };
    persist_and_respond(
        state,
        request,
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
) -> Result<axum::response::Response, AppError> {
    match crate::userapp_builder::inspect_builder_compute(state, &app_id)
        .await
        .map_err(|error| crate::userapp_builder::control_error(&error))?
    {
        crate::userapp_builder::BuilderComputeState::Running(info) => {
            return Ok(HttpResult::success(EnsurePodResponse {
                created: false,
                container_info: PodContainerInfo {
                    container_id: info.container_id,
                    status: info.status,
                },
                message: "UserApp dev 计算容器正在运行；业务服务状态请单独查询".into(),
            })
            .into_response());
        }
        crate::userapp_builder::BuilderComputeState::StoppedRetained => {
            let operation = crate::userapp_builder::compute_control::submit_physical_start(
                state,
                app_id,
                shared_types::UserAppOperationScope::Dev,
                shared_types::UserAppControlRequest {
                    lifecycle_id: None,
                    request_id: None,
                },
            )
            .await
            .map_err(|error| crate::userapp_builder::control_error(&error))?;
            let operation_id = operation.operation_id.clone();
            return Ok((
                axum::http::StatusCode::ACCEPTED,
                HttpResult::success(operation).with_operation_id(operation_id),
            )
                .into_response());
        }
        crate::userapp_builder::BuilderComputeState::StoppedRemoved => {
            // Docker builders use AutoRemove. Their physical object is
            // gone, so there is no captured UID to restart in place. A
            // completed Stop has already drained the old business executor;
            // explicit ensure may create a new container on the same mounts.
            let info = crate::userapp_builder::ensure_userapp_builder(state, &app_id)
                .await
                .map_err(|error| crate::userapp_builder::control_error(&error))?;
            return Ok(HttpResult::success(EnsurePodResponse {
                created: true,
                container_info: PodContainerInfo {
                    container_id: info.container_id,
                    status: info.status,
                },
                message: "UserApp dev 计算容器已使用原工作区重建".into(),
            })
            .into_response());
        }
        crate::userapp_builder::BuilderComputeState::Missing => {}
    }
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
    })
    .into_response())
}

/// Existing prod compute is an explicit physical control, independent of
/// application readiness and the ordinary deployment slot. Missing compute
/// still follows the initial empty-container creation path.
async fn ensure_userapp_prod(
    state: &Arc<AppState>,
    locale: &str,
    app_id: String,
    _user_id: &str,
) -> Result<axum::response::Response, AppError> {
    let runtime = match state.app_service.get_app(&app_id).await {
        Ok(runtime) => runtime,
        Err(app_manager::AppOperationError::NotFound(_)) => {
            return ensure_userapp_prod_created(state, locale, app_id)
                .await
                .map(IntoResponse::into_response);
        }
        Err(e) => {
            // API Server 不可达/RBAC 拒绝等查询故障：语义=查询失败而非应用不存在，
            // 与唤醒路径的 Timeout/Failed 同码（Backend），不触发创建
            error!("[POD_ENSURE] query userapp prod app failed: app_id={app_id}: {e:#}");
            return Ok(HttpResult::<EnsurePodResponse>::error_with_message(
                shared_types::error_codes::ERR_BACKEND_ERROR,
                locale,
                &format!("query userapp prod app failed: {e:#}"),
            )
            .into_response());
        }
    };
    if runtime.replicas > 0 && runtime.phase != "Error" {
        // The physical controller already asks for a Pod. Do not wait on
        // application readiness or an unrelated deploy, but do not race an
        // active Stop/Restart or report a stopped intent as running.
        state
            .userapp_store
            .check_compute_access(&app_id, shared_types::UserAppOperationScope::Prod, false)
            .await
            .map_err(|error| {
                let error: anyhow::Error = error.into();
                crate::userapp_builder::control_error(&error)
            })?;
        return Ok(HttpResult::success(EnsurePodResponse {
            created: false,
            container_info: PodContainerInfo {
                container_id: app_id,
                status: runtime.phase,
            },
            message: "UserApp prod 计算资源已请求运行；业务服务状态请单独查询".into(),
        })
        .into_response());
    }
    let operation = crate::userapp_builder::compute_control::submit_physical_start(
        state,
        app_id,
        shared_types::UserAppOperationScope::Prod,
        shared_types::UserAppControlRequest {
            lifecycle_id: None,
            request_id: None,
        },
    )
    .await
    .map_err(|error| crate::userapp_builder::control_error(&error))?;
    let operation_id = operation.operation_id.clone();
    Ok((
        axum::http::StatusCode::ACCEPTED,
        HttpResult::success(operation).with_operation_id(operation_id),
    )
        .into_response())
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
