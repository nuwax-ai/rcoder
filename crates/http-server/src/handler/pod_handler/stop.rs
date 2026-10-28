use super::helpers::*;
use super::*;

/// 停止计算资源，保留 PVC 和工作区数据。
/// UserApp dev/prod 独立受理，返回 202 与操作查询地址；正在停止时不排队新启动。
/// 普通 agent 沿用删除控制器和 Service 的同步流程。
#[utoipa::path(
    post,
    path = "/computer/pod/stop",
    request_body(content = StopPodRequest, description = "停止容器请求"),
    responses(
        (status = 202, description = "UserApp compute intent accepted; poll status_url. No internal request queue.", body = HttpResult<crate::userapp_builder::compute_control::ComputeOperationView>),
        (status = 200, description = "成功停止容器", body = HttpResult<StopPodResponse>),
        (status = 409, description = "Another compute control is in progress; retry after completion"),
        (status = 400, description = "请求参数无效", body = HttpResult<String>),
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_stop",
    summary = "停止并销毁容器（保留数据卷）",
    description = "根据 user_id / project_id / service_type 定位容器并销毁：K8s 删 STS + Service（PVC 保留，数据不丢，下次 ensure 重建挂回），Docker 删容器。携带 app_id 时进入 userApp 分派：dev=按持久化生命周期与实际计算资源身份停止 UserappBuilder（PVC 与 Service 保留；携带 lifecycle_id/request_id）；prod=scale-to-0 停止生产实例（阻断流量唤醒，ensure 可显式唤醒）。相同停止意图遇到 RecoveryRequired 时沿原 operation_id 异步核验恢复，不另建物理操作；不同控制操作仍快失败。容器不存在时幂等返回成功。注意：对话状态在 agent 内存中，停止即断会话。"
)]
#[instrument(skip(state), fields(user_id = %request.user_id, project_id = %request.project_id))]
pub async fn pod_stop(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(request): I18nJsonOrQuery<StopPodRequest>,
) -> Result<axum::response::Response, AppError> {
    use axum::response::IntoResponse;
    let target = parse_app_target(
        request.app_id.as_deref(),
        request.app_stage.as_deref(),
        request.service_type.as_deref(),
    );
    let (app_id, scope) = match target {
        Ok(AppTarget::Dev(id)) => (id, shared_types::UserAppOperationScope::Dev),
        Ok(AppTarget::Prod(id)) => (id, shared_types::UserAppOperationScope::Prod),
        Ok(AppTarget::NotApp) => {
            return legacy_pod_stop(State(state), I18nJsonOrQuery(request))
                .await
                .map(IntoResponse::into_response);
        }
        Err(error) => {
            return Err(AppError::with_message(
                shared_types::error_codes::ERR_VALIDATION,
                error,
            ));
        }
    };
    let operation = crate::userapp_builder::compute_control::submit(
        &state,
        app_id,
        scope,
        shared_types::ComputeControlAction::Stop,
        shared_types::UserAppControlRequest {
            lifecycle_id: request.lifecycle_id,
            request_id: request.request_id,
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

async fn legacy_pod_stop(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(request): I18nJsonOrQuery<StopPodRequest>,
) -> Result<HttpResult<StopPodResponse>, AppError> {
    let flight = crate::handler::utils::owned_operation_flight(&state)?;
    crate::handler::utils::spawn_owned_operation(flight, async move {
        legacy_pod_stop_inner(state, request).await
    })
    .await
    .map_err(|error| AppError::internal_server_error(&format!("Stop task failed: {error}")))?
}

async fn legacy_pod_stop_inner(
    state: Arc<AppState>,
    request: StopPodRequest,
) -> Result<HttpResult<StopPodResponse>, AppError> {
    let locale = shared_types::current_request_locale();

    // 0. userApp 分派（app_id 存在即短路 agent 流程）
    match parse_app_target(
        request.app_id.as_deref(),
        request.app_stage.as_deref(),
        request.service_type.as_deref(),
    ) {
        Ok(AppTarget::NotApp) => {}
        Ok(AppTarget::Dev(app_id)) => {
            return stop_userapp_dev(
                &state,
                app_id,
                shared_types::UserAppControlRequest {
                    lifecycle_id: request.lifecycle_id,
                    request_id: request.request_id,
                },
            )
            .await;
        }
        Ok(AppTarget::Prod(app_id)) => {
            return stop_userapp_prod(
                &state,
                app_id,
                shared_types::UserAppControlRequest {
                    lifecycle_id: request.lifecycle_id,
                    request_id: request.request_id,
                },
            )
            .await;
        }
        Err(e) => {
            error!("[POD_STOP] invalid app target: {}", e);
            return Ok(invalid_app_target_response(locale, &e));
        }
    }

    // 1. 验证参数
    if let Some(resp) = validate_pod_ids(&request.user_id, &request.project_id, locale, "POD_STOP")
    {
        return Ok(resp);
    }

    // 1.1 解析 service_type
    let service_type = match parse_service_type(request.service_type.as_deref()) {
        Ok(st) => st,
        Err(e) => {
            error!("[POD_STOP] invalid service_type: {}", e);
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                &e,
            ));
        }
    };

    // 1.2 根据 service_type 确定容器标识符
    let container_identifier = container_identifier_for_service(
        &service_type,
        &request.user_id,
        &request.project_id,
        request.pod_id.as_deref(),
    )?;

    info!(
        "[POD_STOP] Stopping container: user_id={}, project_id={}, service_type={}, container_identifier={}",
        request.user_id, request.project_id, service_type, container_identifier
    );

    let admission = state
        .acquire_agent_workload(
            &container_identifier,
            &service_type,
            rcoder_storage::WorkloadAdmissionMode::Exclusive,
        )
        .await
        .map_err(|error| {
            AppError::with_message(
                shared_types::error_codes::ERR_CONFLICT,
                format!("Workload admission unavailable; retry later: {error}"),
            )
        })?;
    let was_existing =
        stop_ordinary_workload(&state, &admission, &container_identifier, &service_type).await?;
    Ok(HttpResult::success(StopPodResponse {
        was_existing,
        message: "Container stopped and destroyed (data volume preserved)".into(),
    }))
}

// ============================================================================
// userApp 分派实现（app_id/app_stage）
// ============================================================================

/// Stop builder compute through the durable, identity-bound lifecycle coordinator.
/// Storage and service identity are retained for a later explicit ensure.
async fn stop_userapp_dev(
    state: &Arc<AppState>,
    app_id: String,
    request: shared_types::UserAppControlRequest,
) -> Result<HttpResult<StopPodResponse>, AppError> {
    let result = crate::userapp_builder::control::execute(state, &app_id, request, false)
        .await
        .map_err(|error| crate::userapp_builder::control_error(&error))?;
    Ok(HttpResult::success(StopPodResponse {
        was_existing: result.was_existing,
        message: "UserApp development compute stopped; workspace data preserved".into(),
    })
    .with_operation_id(result.operation_id))
}

/// stop 的 userApp prod 分支：scale-to-0 停止（阻断流量唤醒；显式 ensure 仍可唤醒）。
async fn stop_userapp_prod(
    state: &Arc<AppState>,
    app_id: String,
    mut request: shared_types::UserAppControlRequest,
) -> Result<HttpResult<StopPodResponse>, AppError> {
    let request_id = request
        .request_id
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
        .clone();
    state
        .app_service
        .stop_app_controlled(&app_id, request)
        .await?;
    let operation = state
        .app_service
        .get_control_operation_by_request(&app_id, &request_id)
        .await?
        .ok_or_else(|| {
            AppError::internal_server_error("Stop completed without a durable operation record")
        })?;
    info!("[POD_STOP] userapp prod stopped: app_id={app_id}");
    Ok(HttpResult::success(StopPodResponse {
        was_existing: true,
        message: "UserApp production instance stopped; traffic wake is disabled".into(),
    })
    .with_operation_id(operation.operation_id))
}
