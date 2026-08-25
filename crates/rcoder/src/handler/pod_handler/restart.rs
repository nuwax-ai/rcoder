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
    description = "根据 user_id 和 project_id 重启容器。如果容器存在，先销毁再创建新容器；如果不存在，直接创建。"
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
            // 原地重启优先（K8s：pod 名/IP/60000 转发地址不变，dev 运行态全保）；
            // Docker 运行时无原地重启（trait 默认 NotImplemented）→ 回落
            // stop + ensure 正路重建（per-app 卷保留，dev 数据不丢）
            let existed = state
                .runtime()
                .get_container_info_by_identifier(&app_id, &ServiceType::UserAppBuilder)
                .await
                .ok()
                .flatten();
            let Some(existing) = existed else {
                return Ok(HttpResult::error_with_message(
                    shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
                    locale,
                    &format!("userapp dev container not found: app_id={app_id}"),
                ));
            };
            let inplace = state
                .runtime()
                .restart_container_inplace(&app_id, &ServiceType::UserAppBuilder)
                .await;
            let (info, message) = match inplace {
                Ok(()) => {
                    info!("[POD_RESTART] userapp dev 容器原地重启完成: app_id={app_id}");
                    (
                        existing.clone(),
                        "UserApp dev 容器已原地重启（地址不变）".to_string(),
                    )
                }
                Err(e) => {
                    info!(
                        "[POD_RESTART] userapp dev 原地重启不可用（Docker 运行时等），回落重建: app_id={app_id}: {e:#}"
                    );
                    state
                        .runtime()
                        .stop_container_by_identifier(&app_id, &ServiceType::UserAppBuilder)
                        .await
                        .map_err(|e| {
                            error!("[POD_RESTART] userapp dev stop failed: app_id={app_id}: {e:#}");
                            AppError::with_message(
                                shared_types::error_codes::ERR_BACKEND_ERROR,
                                format!("userapp dev restart (stop phase) failed: {e:#}"),
                            )
                        })?;
                    // 清注册表 container 字段（防 ensure 命中死注册不重建——
                    // 同 userapp_forward 探活自愈的就地清模式，不 remove_project
                    // 以保 PG 侧 project 行与会话映射）
                    state.shutdown_sse_streams_for_project(&app_id);
                    if let Some(mut info) = state.get_project(&app_id).map(|p| (*p).clone()) {
                        info.set_container(None);
                        if let Err(e) = state.insert_project(app_id.clone(), Arc::new(info)) {
                            warn!(
                                "[POD_RESTART] clear stale container field failed: app_id={app_id}: {e}"
                            );
                        }
                    }
                    let recreated = crate::userapp_publish::agent_runner::ensure_userapp_builder(
                        &state, &app_id,
                    )
                    .await
                    .map_err(|e| {
                        error!("[POD_RESTART] userapp dev recreate failed: app_id={app_id}: {e:#}");
                        AppError::with_message(
                            shared_types::error_codes::ERR_BACKEND_ERROR,
                            format!("userapp dev restart (recreate phase) failed: {e:#}"),
                        )
                    })?;
                    (
                        recreated,
                        "UserApp dev 容器已重建（卷保留，数据不丢）".to_string(),
                    )
                }
            };
            info!("[POD_RESTART] userapp dev 容器重启完成: app_id={app_id}");
            return Ok(HttpResult::success(RestartPodResponse {
                was_existing: true,
                restarted: true,
                container_info: PodContainerInfo {
                    container_id: info.container_id.clone(),
                    status: "Running".to_string(),
                },
                message,
            }));
        }
        Ok(AppTarget::Prod(app_id)) => {
            state.app_service.restart_app(&app_id).await.map_err(|e| {
                error!("[POD_RESTART] userapp prod restart failed: app_id={app_id}: {e:#}");
                AppError::with_message(
                    shared_types::error_codes::ERR_BACKEND_ERROR,
                    format!("userapp prod restart failed: {e:#}"),
                )
            })?;
            info!("[POD_RESTART] userapp prod 滚动重启完成: app_id={app_id}");
            return Ok(HttpResult::success(RestartPodResponse {
                was_existing: true,
                restarted: true,
                container_info: PodContainerInfo {
                    container_id: app_id.clone(),
                    status: "Running".to_string(),
                },
                message: "UserApp 生产实例已滚动重启".to_string(),
            }));
        }
        Err(e) => {
            error!("[POD_RESTART] invalid app target: {}", e);
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_VALIDATION,
                locale,
                &e,
            ));
        }
    }

    // 1. 验证参数
    if request.user_id.trim().is_empty() {
        error!("[POD_RESTART] user_id is required");
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "user_id is required and cannot be empty",
        ));
    }
    if request.project_id.trim().is_empty() {
        error!("[POD_RESTART] project_id is required");
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "project_id is required and cannot be empty",
        ));
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

    // 🆕 优先原地重启（exec SIGTERM PID 1 → kubelet restartPolicy 原地重启容器，卷不 unstage → ~秒级）。
    // 仅容器已存在时尝试；K8s agent-runner 支持。失败（runtime 不支持 / 超时 / agent 卡死）自动
    // 回落下方 destroy+recreate（慢但可靠）。原地重启 pod 名/IP/svc 不变 → VNC backend / 存储
    // 记录 / grpc 池均复用，无需重注或清理。
    if was_existing {
        let runtime = state.runtime().clone();
        match runtime
            .restart_container_inplace(&container_identifier, &service_type)
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
                        .as_ref()
                        .map(|c| PodContainerInfo {
                            container_id: c.container_id.clone(),
                            status: "Running".to_string(),
                        })
                        .unwrap_or_else(|| PodContainerInfo {
                            container_id: container_identifier.clone(),
                            status: "Running".to_string(),
                        }),
                    message: "Container restarted in-place (fast), can access virtual desktop via VNC (Agent service not started)".to_string(),
                };
                info!(
                    "[POD_RESTART] Completed (in-place): container_id={}",
                    response.container_info.container_id
                );
                return Ok(HttpResult::success(response));
            }
            Err(e) => {
                warn!(
                    "[POD_RESTART] 原地重启失败，回落 destroy+recreate: container_identifier={}, err={:?}",
                    container_identifier, e
                );
                // 落入下方 destroy+recreate 兜底
            }
        }
    }

    // 3. 如果容器存在，先销毁
    if let Some(container_info) = existing_container {
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
            .stop_container_by_identifier(&container_identifier, &service_type)
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

        // 物理销毁后，关闭旧容器的 SSE 共享流 + 清理 gRPC 连接（post-destroy；按 addr 关闭幂等，
        // 不依赖 project/session 记录，故可在 delete_container_with_projects 之后）。
        // 地址走 build_grpc_addr（K8s Service FQDN / Docker 容器 IP，与连接建立同源）。
        if shared_types::is_kubernetes_runtime() || !container_info.container_ip.is_empty() {
            let old_grpc_addr = shared_types::build_grpc_addr(
                &container_info.container_name,
                &container_info.container_ip,
                &state.config.app_manager.namespace,
                &state.cluster_domain,
            );
            state.shutdown_sse_streams_by_addr(&old_grpc_addr);
            state.grpc_pool.remove(&old_grpc_addr).await;
        }

        // 验证容器是否真正移除
        let mut deletion_confirmed = false;

        for i in 0..10 {
            // 最多等待 5 秒 (10 * 500ms)
            match runtime
                .find_container(&container_identifier, &service_type)
                .await
            {
                Ok(Some(_)) => {
                    if i == 0 {
                        info!(
                            "[POD_RESTART] Container still exists, waiting for cleanup: container_identifier={}",
                            container_identifier
                        );
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                }
                Ok(None) => {
                    info!(
                        "[POD_RESTART] Confirmed container removed: container_identifier={}",
                        container_identifier
                    );
                    deletion_confirmed = true;
                    break;
                }
                Err(e) => {
                    warn!(
                        "[POD_RESTART] check container removed status: {}, container already removed",
                        e
                    );
                    // 如果是其他错误，也可能意味着Container status abnormal，尝试继续
                    deletion_confirmed = true;
                    break;
                }
            }
        }

        if !deletion_confirmed {
            warn!(
                "[POD_RESTART] Wait for container removal timeout, subsequent creation may fail: container_identifier={}",
                container_identifier
            );
        }
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
        service_type: service_type.clone(),
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
            info.set_service_type(Some(service_type.clone()));
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
