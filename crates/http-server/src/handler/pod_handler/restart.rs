use super::helpers::*;
use super::*;

/// 重启计算资源，先确认旧实例退出，再启动同一工作区的新运行实例。
/// UserApp 返回 202 和原操作身份；普通 agent 保留既有同步重启路径。
#[utoipa::path(
    post,
    path = "/computer/pod/restart",
    request_body(content = RestartPodRequest, description = "重启容器请求"),
    responses(
        (status = 202, description = "UserApp compute intent accepted; poll status_url. No internal request queue.", body = HttpResult<crate::userapp_builder::compute_control::ComputeOperationView>),
        (status = 200, description = "成功重启容器", body = HttpResult<RestartPodResponse>),
        (status = 409, description = "Another compute control is in progress; no internal queue"),
        (status = 400, description = "请求参数无效", body = HttpResult<String>),
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_restart",
    summary = "重启计算资源并保留数据",
    description = "UserApp dev/prod requests use app_id and app_stage, with optional lifecycle_id and request_id. Normally returns HTTP 202 with operation_id and status_url. In Kubernetes mode the restarted dev StatefulSet and prod Deployment use the current configured platform image while retaining the workspace PVC. Docker dev builders removed by AutoRemove after a completed Stop are recreated on the same mounts and return HTTP 200. With default recovery enabled, missing K8s dev/prod workloads with verified retained PVCs are reconstructed through the existing ensure/deployment coordinator and return HTTP 200 with RestartPodResponse and the admitted operation identity when available. A retry of the same durable intent in RecoveryRequired resumes verification under the original operation_id; an explicitly different request_id remains a conflict. Stop has priority over Restart; new Start/Restart during Stop is rejected without queuing. Ordinary business slots do not block admission to the durable compute coordinator. Unknown writes remain visible as recovery_required. Ordinary agent restart behavior is unchanged."
)]
#[instrument(skip(state), fields(user_id = %request.user_id, project_id = %request.project_id))]
pub async fn pod_restart(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(request): I18nJsonOrQuery<RestartPodRequest>,
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
            return legacy_pod_restart(State(state), I18nJsonOrQuery(request))
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
    use crate::userapp_builder::BuilderComputeState;
    let recreate_dev = match scope {
        shared_types::UserAppOperationScope::Dev => {
            match crate::userapp_builder::inspect_builder_compute(&state, &app_id)
                .await
                .map_err(|error| crate::userapp_builder::control_error(&error))?
            {
                BuilderComputeState::StoppedRemoved => true,
                BuilderComputeState::Missing => {
                    state.config.userapp_storage.default_recovery_enabled
                        && state
                            .runtime()
                            .discover_retained_app_storage(
                                &app_id,
                                shared_types::UserAppOperationScope::Dev,
                            )
                            .await
                            .map_err(|error| crate::userapp_builder::control_error(&error.into()))?
                            .is_some()
                }
                BuilderComputeState::Running(_) | BuilderComputeState::StoppedRetained => false,
            }
        }
        shared_types::UserAppOperationScope::Prod => false,
        shared_types::UserAppOperationScope::Application => {
            return Err(AppError::bad_request(
                "Compute restart requires dev or prod",
            ));
        }
    };
    if recreate_dev {
        // A completed Docker Stop or an enabled retained-PVC K8s recovery has
        // no compute UID to restart. The existing ensure owns reconstruction.
        if let Some(expected) = request.lifecycle_id.as_deref() {
            let current = state
                .userapp_store
                .get_application(&app_id)
                .await
                .map_err(|error| {
                    let error: anyhow::Error = error.into();
                    crate::userapp_builder::control_error(&error)
                })?;
            if current.as_ref().map(|app| app.lifecycle_id.as_str()) != Some(expected) {
                return Err(AppError::with_message(
                    shared_types::error_codes::ERR_CONFLICT,
                    "Application lifecycle changed before compute restart",
                ));
            }
        }
        let info = crate::userapp_builder::ensure_userapp_builder(&state, &app_id)
            .await
            .map_err(|error| crate::userapp_builder::control_error(&error))?;
        return Ok(HttpResult::success(RestartPodResponse {
            was_existing: false,
            restarted: true,
            container_info: PodContainerInfo {
                container_id: info.container_id,
                status: info.status,
            },
            message: "UserApp dev 计算容器已使用原工作区重建".into(),
        })
        .into_response());
    }
    if scope == shared_types::UserAppOperationScope::Prod
        && state.config.userapp_storage.default_recovery_enabled
        && state
            .runtime()
            .discover_retained_app_storage(&app_id, scope)
            .await
            .map_err(|error| crate::userapp_builder::control_error(&error.into()))?
            .is_some()
        && state
            .runtime()
            .get_deployment_status(&app_id)
            .await
            .map_err(|error| crate::userapp_builder::control_error(&error.into()))?
            .is_none()
    {
        let result = state
            .app_service
            .restart_app_enhanced(
                &app_id,
                app_manager::models::StartAppRequest {
                    lifecycle_id: request.lifecycle_id,
                    request_id: request.request_id,
                    ..Default::default()
                },
            )
            .await
            .map_err(AppError::from)?;
        // Preserve the existing 200 response shape and original operation identity.
        let mut response = HttpResult::success(RestartPodResponse {
            was_existing: false,
            restarted: true,
            container_info: PodContainerInfo {
                container_id: app_id,
                status: result.runtime.phase,
            },
            message: "UserApp production compute restored from its retained workspace".into(),
        });
        if let Some(operation_id) = result.operation_id {
            response = response.with_operation_id(operation_id);
        }
        return Ok(response.into_response());
    }
    let operation = crate::userapp_builder::compute_control::submit(
        &state,
        app_id,
        scope,
        shared_types::ComputeControlAction::Restart,
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

async fn legacy_pod_restart(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(request): I18nJsonOrQuery<RestartPodRequest>,
) -> Result<HttpResult<RestartPodResponse>, AppError> {
    let locale = shared_types::current_request_locale();
    let flight = crate::handler::utils::owned_operation_flight(&state)?;
    crate::handler::utils::spawn_owned_operation(flight, async move {
        legacy_pod_restart_inner(state, request, locale).await
    })
    .await
    .map_err(|error| AppError::internal_server_error(&format!("Restart task failed: {error}")))?
}

async fn legacy_pod_restart_inner(
    state: Arc<AppState>,
    request: RestartPodRequest,
    locale: &'static str,
) -> Result<HttpResult<RestartPodResponse>, AppError> {
    // 0. userApp 分派（app_id 存在即短路 agent 流程）
    match parse_app_target(
        request.app_id.as_deref(),
        request.app_stage.as_deref(),
        request.service_type.as_deref(),
    ) {
        Ok(AppTarget::NotApp) => {}
        Ok(AppTarget::Dev(app_id)) => {
            return restart_userapp_dev(
                &state,
                app_id,
                shared_types::UserAppControlRequest {
                    lifecycle_id: request.lifecycle_id.clone(),
                    request_id: request.request_id.clone(),
                },
            )
            .await;
        }
        Ok(AppTarget::Prod(app_id)) => {
            return restart_userapp_prod(
                &state,
                app_id,
                shared_types::UserAppControlRequest {
                    lifecycle_id: request.lifecycle_id.clone(),
                    request_id: request.request_id.clone(),
                },
            )
            .await;
        }
        Err(e) => {
            error!("[POD_RESTART] invalid app target: {}", e);
            return Ok(invalid_app_target_response(locale, &e));
        }
    }

    // 1. 验证参数
    if let Some(resp) =
        validate_pod_ids(&request.user_id, &request.project_id, locale, "POD_RESTART")
    {
        return Ok(resp);
    }

    // 1.1 验证资源限制
    if let Some(ref limits) = request.resource_limits
        && let Err(e) = validate_resource_limits(limits)
    {
        error!("[POD_RESTART] resources update failed: {}", e);
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
            error!("[POD_RESTART] invalid service_type: {}", e);
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
        "[POD_RESTART] Restarting container: user_id={}, project_id={}, service_type={}, container_identifier={}",
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
    // A logical-name in-place restart cannot provide the captured UID fence.
    // In particular an unknown reply must never fall through to destructive
    // recreation. Both explicit operations now share the exact stop contract.
    let was_existing =
        stop_ordinary_workload(&state, &admission, &container_identifier, &service_type).await?;
    let ensure_request = EnsurePodRequest {
        user_id: request.user_id,
        project_id: request.project_id,
        resource_limits: request.resource_limits,
        pod_id: request.pod_id,
        tenant_id: request.tenant_id,
        space_id: request.space_id,
        isolation_type: request.isolation_type,
        service_type: request.service_type,
        app_id: None,
        app_stage: None,
    };
    let ensured = ensure_ordinary_pod_with_admission(
        &state,
        &ensure_request,
        locale,
        &admission,
        &container_identifier,
        &service_type,
    )
    .await?;
    let container_info = ensured
        .data
        .ok_or_else(|| {
            AppError::internal_server_error(
                "Restart creation completed without a confirmed container registration",
            )
        })?
        .container_info;
    Ok(HttpResult::success(RestartPodResponse {
        was_existing,
        restarted: true,
        container_info,
        message: "Container restarted; workspace data preserved".into(),
    }))
}

// ============================================================================
// userApp 分派实现（app_id/app_stage）
// ============================================================================

/// Restart the captured builder compute identity through durable admission.
/// Unknown runtime failures never trigger destructive stop-and-create fallback.
async fn restart_userapp_dev(
    state: &Arc<AppState>,
    app_id: String,
    request: shared_types::UserAppControlRequest,
) -> Result<HttpResult<RestartPodResponse>, AppError> {
    let result = crate::userapp_builder::control::execute(state, &app_id, request, true)
        .await
        .map_err(|error| crate::userapp_builder::control_error(&error))?;
    let info = result.container.ok_or_else(|| {
        AppError::internal_server_error("Builder restart completed without a container identity")
            .with_operation_id(result.operation_id.clone())
    })?;
    Ok(HttpResult::success(RestartPodResponse {
        was_existing: result.was_existing,
        restarted: true,
        container_info: PodContainerInfo {
            container_id: info.container_id,
            status: "Running".into(),
        },
        message: "UserApp development compute restarted; workspace data preserved".into(),
    })
    .with_operation_id(result.operation_id))
}

/// restart 的 userApp prod 分支：滚动重启（rollout）。
async fn restart_userapp_prod(
    state: &Arc<AppState>,
    app_id: String,
    mut request: shared_types::UserAppControlRequest,
) -> Result<HttpResult<RestartPodResponse>, AppError> {
    let request_id = request
        .request_id
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
        .clone();
    state
        .app_service
        .restart_app_controlled(&app_id, request)
        .await?;
    let operation = state
        .app_service
        .get_control_operation_by_request(&app_id, &request_id)
        .await?;
    info!(app_id, "UserApp production restart completed");
    let mut response = HttpResult::success(RestartPodResponse {
        was_existing: true,
        restarted: true,
        container_info: PodContainerInfo {
            container_id: app_id,
            status: "Running".into(),
        },
        message: "UserApp production instance restarted".into(),
    });
    if let Some(operation) = operation {
        response = response.with_operation_id(operation.operation_id);
    }
    Ok(response)
}
