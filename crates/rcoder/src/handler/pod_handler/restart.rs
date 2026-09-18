use super::helpers::*;
use super::*;

/// 重启容器（销毁后重建）
///
/// 根据 user_id 和 project_id 重启容器。
/// 如果容器存在，先销毁再创建新容器；如果不存在，直接创建。
#[utoipa::path(
    post,
    path = "/computer/pod/restart",
    request_body(content = RestartPodRequest, description = "重启容器请求"),
    responses(
        (status = 200, description = "成功重启容器", body = HttpResult<RestartPodResponse>),
        (status = 400, description = "请求参数无效", body = HttpResult<String>),
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_restart",
    summary = "重启容器（销毁后重建）",
    description = "Agent requests restart or recreate their container. UserApp dev requests carry user_id, lifecycle_id and request_id into durable admission, restart captured compute identity, retain workspace storage, and never recreate after an uncertain restart error. UserApp prod requests use the production lifecycle coordinator."
)]
#[instrument(skip(state), fields(user_id = %request.user_id, project_id = %request.project_id))]
pub async fn pod_restart(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(request): I18nJsonOrQuery<RestartPodRequest>,
) -> Result<HttpResult<RestartPodResponse>, AppError> {
    let locale = shared_types::current_request_locale();

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

    // 2. 检查容器是否存在
    let existing_container = ComputerContainerManager::get_container_info_with_type(
        &container_identifier,
        state.runtime(),
        &service_type,
    )
    .await?;
    let was_existing = existing_container.is_some();
    // 🆕 优先原地重启（快路径，容器已存在时尝试；失败回落 destroy+recreate）
    if was_existing
        && let Some(response) = try_restart_inplace(
            &state,
            &container_identifier,
            &service_type,
            existing_container.as_ref(),
        )
        .await
    {
        return Ok(response);
    }

    // 3. 容器存在时先销毁（存储记录 + 物理容器 + SSE/gRPC 连接 + 确认移除）
    if let Some(container_info) = existing_container {
        destroy_for_recreate(
            &state,
            &container_info,
            &container_identifier,
            &service_type,
        )
        .await;
    }

    // 4. 定义资源限制（API 入参优先，缺失字段回退 configmap 默认值）
    let resource_limits =
        resolve_resource_limits_from_config(&state, &service_type, request.resource_limits)?;

    // 5. 强制创建新容器
    info!(
        "[POD_RESTART] Force creating new container: container_identifier={}, service_type={}",
        container_identifier, service_type
    );

    let options = ContainerCreateOptions {
        user_id: request.user_id.clone(),
        project_id: request.project_id.clone(),
        resource_limits,
        pod_id: request.pod_id.clone(),
        isolation_type: request.isolation_type.clone(),
        tenant_id: request.tenant_id.clone(),
        space_id: request.space_id.clone(),
        service_type,
    };
    let container_info = ComputerContainerManager::get_or_create_container_for_user_with_type(
        &options,
        state.runtime(),
    )
    .await?;

    info!(
        "[POD_RESTART] New container created successfully: container_id={}",
        container_info.container_id
    );

    // 5. 注册 VNC backend 到 pingora(新容器已创建,覆盖旧地址)
    register_vnc_backend(&state, &request.user_id, &container_info, &service_type);

    // 6. 在 存储中记录容器信息
    {
        // 🛡️ 关键修复：如果项目已存在，保留现有的 session_id
        let project_info = if let Some(existing) = state.get_project(&request.project_id) {
            // 项目已存在，只更新容器信息，保留 session_id 等状态
            let mut info = (*existing).clone();
            info.set_container(Some(container_info.clone()));
            info
        } else {
            // 项目不存在，创建新记录
            let mut info = ProjectAndContainerInfo::new(request.project_id.clone());
            info.set_user_id(Some(request.user_id.clone()));
            info.set_pod_id(request.pod_id.clone());
            info.set_service_type(Some(service_type));
            info.set_scope(
                request.tenant_id.clone(),
                request.space_id.clone(),
                request.isolation_type.clone(),
            );
            info.set_container(Some(container_info.clone()));
            info
        };
        state
            .insert_project(request.project_id.clone(), Arc::new(project_info))
            .map_err(|e| {
                tracing::error!("[STORAGE] insert_project failed: {}", e);
                e
            })?;
    }

    // 7. 构建响应
    let pod_container_info = PodContainerInfo {
        container_id: container_info.container_id.clone(),
        status: container_info.status.clone(),
    };

    let message = if was_existing {
        "Container restarted, can access virtual desktop via VNC (Agent service not started)"
            .to_string()
    } else {
        "Container created (previously did not exist), can access virtual desktop via VNC (Agent service not started)".to_string()
    };

    let response = RestartPodResponse {
        was_existing,
        restarted: true,
        container_info: pod_container_info,
        message,
    };

    info!(
        "[POD_RESTART] Completed: was_existing={}, container_id={}",
        was_existing, container_info.container_id
    );

    Ok(HttpResult::success(response))
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

// ============================================================================
// 主流程阶段实现（从 pod_restart 拆出，控制流/日志逐条保持）
// ============================================================================

/// 原地重启快路径：exec SIGTERM PID 1 → kubelet restartPolicy 原地重启容器，
/// 卷不 unstage → ~秒级。仅容器已存在时尝试；K8s agent-runner 支持。失败
/// （runtime 不支持 / 超时 / agent 卡死）返回 None 自动回落 destroy+recreate
/// （慢但可靠）。原地重启 pod 名/IP/svc 不变 → VNC backend / 存储记录 /
/// grpc 池均复用，无需重注或清理。
async fn try_restart_inplace(
    state: &Arc<AppState>,
    container_identifier: &str,
    service_type: &ServiceType,
    existing_container: Option<&ContainerBasicInfo>,
) -> Option<HttpResult<RestartPodResponse>> {
    let runtime = state.runtime().clone();
    match runtime
        .restart_container_inplace(container_identifier, service_type)
        .await
    {
        Ok(()) => {
            info!(
                "[POD_RESTART] Agent 原地重启完成（fast，volume 未 unstage）: container_identifier={}",
                container_identifier
            );
            // 原地重启不换 pod（同 UID）→ 复用 existing_container 的 container_id；
            // status=Running（in-place 的 poll 已确认 ready）。不再 re-fetch —— 避免 fetch 失败
            // 时回落 destroy 把刚原地重启好的 pod 毁掉重建（违背原地重启初衷）。
            let response = RestartPodResponse {
                was_existing: true,
                restarted: true,
                container_info: existing_container
                    .map(|c| PodContainerInfo {
                        container_id: c.container_id.clone(),
                        status: "Running".to_string(),
                    })
                    .unwrap_or_else(|| PodContainerInfo {
                        container_id: container_identifier.to_string(),
                        status: "Running".to_string(),
                    }),
                message: "Container restarted in-place (fast), can access virtual desktop via VNC (Agent service not started)".to_string(),
            };
            info!(
                "[POD_RESTART] Completed (in-place): container_id={}",
                response.container_info.container_id
            );
            Some(HttpResult::success(response))
        }
        Err(e) => {
            warn!(
                "[POD_RESTART] 原地重启失败，回落 destroy+recreate: container_identifier={}, err={:?}",
                container_identifier, e
            );
            // 落入 destroy+recreate 兜底
            None
        }
    }
}

/// 销毁既有容器为重建铺路：存储级删除（含关联 project 记录）→ 物理停止 →
/// SSE/gRPC 连接清理 → 轮询确认移除（最多 5s）。
///
/// 停止失败不阻断（记日志继续创建新容器——良性竞态由后续 recreate 报错兜底）。
async fn destroy_for_recreate(
    state: &Arc<AppState>,
    container_info: &ContainerBasicInfo,
    container_identifier: &str,
    service_type: &ServiceType,
) {
    info!(
        "[POD_RESTART] Destroying existing container: container_id={}",
        container_info.container_id
    );

    // 从存储中彻底移除旧容器及其所有关联记录
    // 使用 container_id 删除,确保清理该容器关联的所有 project_id
    let (container_deleted, deleted_projects) = state
        .delete_container_with_projects_durable(&container_info.container_id)
        .await;
    info!(
        "[POD_RESTART] Cleaned up old container records: container_id={}, container_deleted={}, deleted_projects={}",
        container_info.container_id, container_deleted, deleted_projects
    );

    let runtime = state.runtime().clone();

    // 使用 pod_id 优先的标识符停止容器（与创建时一致）
    if let Err(e) = runtime
        .stop_container_by_identifier(container_identifier, service_type)
        .await
    {
        // 记录错误但继续尝试创建新容器
        error!(
            "[POD_RESTART] Failed to stop container (will continue creating new container): container_id={}, error={}",
            container_info.container_id, e
        );
    } else {
        info!(
            "[POD_RESTART] Container destroyed: container_id={}",
            container_info.container_id
        );
    }

    // 物理销毁后，关闭旧容器的 SSE 共享流 + 清理 gRPC 连接（post-destroy；
    // delete_container_with_projects 之后的既有顺序）
    state
        .teardown_container_connections(
            &container_info.container_name,
            &container_info.container_ip,
        )
        .await;

    // 验证容器是否真正移除（最多等待 5 秒）
    let deletion_confirmed =
        confirm_container_removed(&runtime, container_identifier, service_type, "POD_RESTART")
            .await;
    if !deletion_confirmed {
        warn!(
            "[POD_RESTART] Wait for container removal timeout, subsequent creation may fail: container_identifier={}",
            container_identifier
        );
    }
}
