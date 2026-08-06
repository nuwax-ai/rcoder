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

    // 1. 验证参数
    if request.user_id.trim().is_empty() {
        error!("[POD_ENSURE] user_id is required");
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "user_id is required and cannot be empty",
        ));
    }
    if request.project_id.trim().is_empty() {
        error!("[POD_ENSURE] project_id is required");
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
        " [POD_ENSURE] Ensuring container exists: user_id={}, project_id={}, service_type={}, container_identifier={}",
        request.user_id, request.project_id, service_type, container_identifier
    );

    // === 并发保护：检查是否有其他请求正在创建同一用户的容器 ===
    // 使用原子标记（DashMap）避免并发请求互相干扰，无死锁风险

    // 🚀 关键修复：先订阅 broadcast channel，再检查 pod_creating
    // 避免 subscribe-after-send 竞态：如果在检查 pod_creating 之后才订阅，
    // 创建者可能已经移除了标记并发送了通知，导致我们错过消息。
    let mut rx = state.pod_created_tx.subscribe();

    // view() 在闭包返回后立即释放锁，无 Ref 暴露
    if let Some(elapsed) = state
        .pod_creating
        .view(&container_identifier, |_, t| t.elapsed())
    {
        // 标记超过 60 秒视为过期（创建方可能已崩溃），忽略并继续
        if elapsed < std::time::Duration::from_secs(60) {
            info!(
                " [POD_ENSURE] Container is being created, waiting for completion: container_identifier={}, elapsed={:?}",
                container_identifier, elapsed
            );

            let mut waited_container_info = None;

            match tokio::time::timeout(std::time::Duration::from_secs(30), async {
                loop {
                    match rx.recv().await {
                        Ok(created_user_id) if created_user_id == container_identifier => {
                            // 我们等待的容器已创建
                            break;
                        }
                        Ok(_) => continue, // 其他用户的容器，继续等待
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            // 通道关闭，退出
                            break;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // 消息丢失，检查标记是否已移除
                            if !state.pod_creating.contains_key(&container_identifier) {
                                break;
                            }
                            continue;
                        }
                    }
                }
            })
            .await
            {
                Ok(_) => {
                    // 容器创建成功，获取容器信息
                    if let Ok(Some(info)) = state
                        .runtime()
                        .get_container_info_by_identifier(&container_identifier, &service_type)
                        .await
                    {
                        info!(
                            " [POD_ENSURE] Wait succeeded, container ready: container_identifier={}, container_id={}",
                            container_identifier, info.container_id
                        );
                        waited_container_info = Some(info);
                    }
                }
                Err(_) => {
                    // 超时处理
                    warn!(
                        " [POD_ENSURE] Wait for container creation timeout (30s): container_identifier={}",
                        container_identifier
                    );
                }
            }

            // 如果等待成功，直接使用已就绪的容器，跳过创建流程
            if let Some(info) = waited_container_info {
                // 容器已就绪(由并发请求创建),注册 VNC backend
                register_vnc_backend(&state, &request.user_id, &info, &service_type);

                // 更新存储 记录
                let project_info = if let Some(existing) = state.get_project(&request.project_id) {
                    let mut pinfo = (*existing).clone();
                    pinfo.set_container(Some(info.clone()));
                    pinfo
                } else {
                    let mut pinfo = ProjectAndContainerInfo::new(request.project_id.clone());
                    // 入口尽可能记录完整信息（user_id 对两类业务都记录）；
                    // 是否参与 user_id 查找由 service_type 在使用方区分（见 adapter 索引门控与 find_projects_by_user_id）。
                    pinfo.set_user_id(Some(request.user_id.clone()));
                    pinfo.set_pod_id(request.pod_id.clone());
                    pinfo.set_service_type(Some(service_type.clone()));
                    pinfo.set_scope(
                        request.tenant_id.clone(),
                        request.space_id.clone(),
                        request.isolation_type.clone(),
                    );
                    pinfo.set_container(Some(info.clone()));
                    pinfo
                };
                state
                    .insert_project(request.project_id.clone(), Arc::new(project_info))
                    .map_err(|e| {
                        tracing::error!("[STORAGE] insert_project failed: {}", e);
                        e
                    })?;
                debug!(
                    " [POD_ENSURE] project record updated: project_id={}, user_id={}, container_id={}",
                    request.project_id, request.user_id, info.container_id
                );

                // 返回成功响应
                let pod_container_info = PodContainerInfo {
                    container_id: info.container_id.clone(),
                    status: info.status.clone(),
                };
                return Ok(HttpResult::success(EnsurePodResponse {
                    created: false,
                    container_info: pod_container_info,
                    message: format!(
                        "Container ready (waiting for other request to complete creation): container_id={}",
                        info.container_id
                    ),
                }));
            }
            // 等待超时，继续正常的创建流程（此时标记可能已过期被清理）
            warn!(
                " [POD_ENSURE] Wait for container creation timeout (30s), will continue to try creating: container_identifier={}",
                container_identifier
            );
        } else {
            // 标记过期，清理后继续
            warn!(
                " [POD_ENSURE] Creation mark expired ({:?}), cleaning up and continuing",
                elapsed
            );
            state.pod_creating.remove(&container_identifier);
        }
    }

    // 2. 🔍 实时查询 runtime 检查容器是否存在（不依赖缓存）
    let runtime = state.runtime().clone();

    let existing_container = runtime
        .find_container(&container_identifier, &service_type)
        .await
        .map_err(|e| {
            error!("[POD_ENSURE] Failed to query container status: {}", e);
            AppError::internal_server_error(&format!("Failed to query container status: {}", e))
        })?;

    // 判断是否需要创建新容器
    let need_create = match existing_container {
        Some(result) if result.status == container_runtime_api::ContainerRuntimeStatus::Running => {
            // 容器存在且正在运行，无需创建
            info!(
                " [POD_ENSURE] Container already exists and running: container_id={}, status={:?}",
                result.container_id, result.status
            );
            false
        }
        Some(result) => {
            // 容器存在但未运行（Exited 等状态），需要删除并重建
            warn!(
                " [POD_ENSURE] Container exists but not running: container_id={}, status={:?}, will delete and recreate",
                result.container_id, result.status
            );

            // 关闭指向旧容器的 SSE 共享流（stop 前调用，此时容器信息尚全；按 addr 关闭幂等）。
            // 地址必须走 build_grpc_addr（K8s 用 Service FQDN，与 registry 中存储的地址同源）；
            // K8s 不依赖 container_ip，Docker 下 ip 为空才跳过。
            if shared_types::is_kubernetes_runtime() || !result.container_ip.is_empty() {
                let sse_grpc_addr = shared_types::build_grpc_addr(
                    &result.container_name,
                    &result.container_ip,
                    &state.config.app_manager.namespace,
                    &state.cluster_domain,
                );
                state.shutdown_sse_streams_by_addr(&sse_grpc_addr);
            }

            // 删除旧容器（使用 pod_id 优先的标识符，与创建时一致）
            // 如果删除失败（包括容器不存在等情况），返回错误让调用者知道
            runtime
                .stop_container_by_identifier(&container_identifier, &service_type)
                .await
                .map_err(|e| {
                    error!(
                        " [POD_ENSURE] Failed to delete old container: container_id={}, error={}",
                        result.container_id, e
                    );
                    AppError::internal_server_error(&format!(
                        "Failed to delete old container: {}",
                        e
                    ))
                })?;

            info!(
                " [POD_ENSURE] Old container deleted: container_id={}",
                result.container_id
            );

            // 清理旧容器的 gRPC 连接
            // 地址与连接建立时同源：K8s 用 Service FQDN，Docker 用容器 IP
            if shared_types::is_kubernetes_runtime() || !result.container_ip.is_empty() {
                let old_grpc_addr = shared_types::build_grpc_addr(
                    &result.container_name,
                    &result.container_ip,
                    &state.config.app_manager.namespace,
                    &state.cluster_domain,
                );
                state.grpc_pool.remove(&old_grpc_addr).await;
            }

            // ⏱️ 等待 Docker 完全释放容器资源（避免竞态条件）
            // Docker 删除是异步操作，立即创建同名容器可能导致资源冲突
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            debug!(" [POD_ENSURE] container resources already released");

            true
        }
        None => {
            // 容器不存在，需要创建
            info!(" [POD_ENSURE] container not found, will create new container");
            true
        }
    };

    // 3. 获取或创建容器（带重试机制 + 标记）
    let (container_info, created) = if need_create {
        // 🆕 设置创建标记，防止并发请求重复创建
        let create_started = Instant::now();
        state
            .pod_creating
            .insert(container_identifier.clone(), create_started);

        info!(
            " [POD_ENSURE] Creation marker set: container_identifier={}, user_id={}, project_id={}, max_attempts=3",
            container_identifier, request.user_id, request.project_id
        );

        // 创建新容器，最多重试 3 次
        let resource_limits =
            resolve_resource_limits_from_config(&state, &service_type, request.resource_limits)?;

        let mut last_error = None;
        let mut result = None;
        let max_attempts = 3;

        for attempt in 1..=max_attempts {
            let attempt_started = Instant::now();
            info!(
                " [POD_ENSURE] Container creation attempt {}/{} started: container_identifier={}, elapsed_since_marker={:?}",
                attempt,
                max_attempts,
                container_identifier,
                create_started.elapsed()
            );

            let options = ContainerCreateOptions {
                user_id: request.user_id.clone(),
                project_id: request.project_id.clone(),
                resource_limits: resource_limits.clone(),
                pod_id: request.pod_id.clone(),
                isolation_type: request.isolation_type.clone(),
                tenant_id: request.tenant_id.clone(),
                space_id: request.space_id.clone(),
                service_type: service_type.clone(),
            };
            match ComputerContainerManager::get_or_create_container_for_user_with_type(
                &options,
                state.runtime(),
            )
            .await
            {
                Ok(info) => {
                    info!(
                        " [POD_ENSURE] Container created successfully (attempt {}): container_id={}, ip={}, attempt_elapsed={:?}, total_elapsed={:?}",
                        attempt,
                        info.container_id,
                        info.container_ip,
                        attempt_started.elapsed(),
                        create_started.elapsed()
                    );
                    result = Some(info);
                    break;
                }
                Err(e) => {
                    last_error = Some(e);
                    if attempt < max_attempts {
                        warn!(
                            " [POD_ENSURE] Container creation failed (attempt {}/{}), will retry: error={}, attempt_elapsed={:?}, total_elapsed={:?}",
                            attempt,
                            max_attempts,
                            last_error
                                .as_ref()
                                .map(|e| e.to_string())
                                .unwrap_or_else(|| "Unknown error".to_string()),
                            attempt_started.elapsed(),
                            create_started.elapsed()
                        );
                        // 等待一段时间后重试（指数退避）
                        tokio::time::sleep(tokio::time::Duration::from_millis(
                            200 * attempt as u64,
                        ))
                        .await;
                    } else {
                        error!(
                            "[POD_ENSURE] Container creation failed after {} attempts: error={}, total_elapsed={:?}",
                            max_attempts,
                            last_error
                                .as_ref()
                                .map(|e| e.to_string())
                                .unwrap_or_else(|| "Unknown error".to_string()),
                            create_started.elapsed()
                        );
                    }
                }
            }
        }

        // 返回结果或错误
        match result {
            Some(info) => {
                debug!(
                    " [POD_ENSURE] Clearing creation marker after success: container_identifier={}, total_elapsed={:?}",
                    container_identifier,
                    create_started.elapsed()
                );
                // 创建成功，清除标记
                state.pod_creating.remove(&container_identifier);
                // 🚀 发送容器创建完成通知（唤醒等待方）
                let _ = state.pod_created_tx.send(container_identifier.clone());
                (info, true)
            }
            None => {
                debug!(
                    " [POD_ENSURE] Clearing creation marker after failure: container_identifier={}, total_elapsed={:?}",
                    container_identifier,
                    create_started.elapsed()
                );
                // 创建失败，也要清除标记
                state.pod_creating.remove(&container_identifier);
                // 直接返回原始错误，保留具体的错误信息
                return Err(last_error.unwrap_or_else(|| {
                    AppError::internal_server_error(
                        "Container creation failed but no error info captured",
                    )
                }));
            }
        }
    } else {
        // 获取现有容器的完整信息
        match runtime
            .get_container_info_by_identifier(&container_identifier, &service_type)
            .await
        {
            Ok(Some(info)) => {
                // 容器信息正常获取
                (info, false)
            }
            Ok(None) => {
                // Docker API 确认容器在运行，但内部 map 还没同步
                // 短暂等待让内部 map 同步，而不是直接重建
                warn!(
                    " [POD_ENSURE] Container running but internal mapping not ready, waiting for sync: container_identifier={}",
                    container_identifier
                );

                let mut retry_info = None;
                for retry_attempt in 1..=3 {
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    match runtime
                        .get_container_info_by_identifier(&container_identifier, &service_type)
                        .await
                    {
                        Ok(Some(info)) => {
                            info!(
                                " [POD_ENSURE] Internal mapping synced (retry {}): container_id={}",
                                retry_attempt, info.container_id
                            );
                            retry_info = Some(info);
                            break;
                        }
                        _ => {
                            debug!("[POD_ENSURE] Mapping not found: retry {}", retry_attempt);
                        }
                    }
                }

                match retry_info {
                    Some(info) => (info, false),
                    None => {
                        // 3次重试后仍失败，才考虑重建
                        warn!(
                            " [POD_ENSURE] Wait for sync timeout, attempting to recreate: container_identifier={}",
                            container_identifier
                        );

                        let resource_limits = resolve_resource_limits_from_config(
                            &state,
                            &service_type,
                            request.resource_limits,
                        )?;

                        // 设置创建标记
                        state
                            .pod_creating
                            .insert(container_identifier.clone(), Instant::now());

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
                        let result =
                            ComputerContainerManager::get_or_create_container_for_user_with_type(
                                &options,
                                state.runtime(),
                            )
                            .await;

                        // 清除创建标记
                        state.pod_creating.remove(&container_identifier);

                        // 🚀 发送容器创建完成通知（唤醒等待方）
                        if result.is_ok() {
                            let _ = state.pod_created_tx.send(container_identifier.clone());
                        }

                        match result {
                            Ok(info) => {
                                info!(
                                    " [POD_ENSURE] Container recreated successfully: container_id={}",
                                    info.container_id
                                );
                                (info, true)
                            }
                            Err(e) => {
                                error!(
                                    " [POD_ENSURE] Container recreation failed: container_identifier={}, error={}",
                                    container_identifier, e
                                );
                                return Err(e);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error!(
                    " [POD_ENSURE] Failed to get container full info: container_identifier={}, error={}",
                    container_identifier, e
                );
                return Err(AppError::internal_server_error(&format!(
                    "Failed to get container full info: {}",
                    e
                )));
            }
        }
    };

    // 4. 注册 VNC backend 到 pingora(容器已就绪,无论新建还是复用)
    register_vnc_backend(&state, &request.user_id, &container_info, &service_type);

    // 5. 更新存储中的容器信息（用于后续保活）
    // 无论容器是新建还是已存在，都要确保 存储 记录是最新的
    let project_info = if let Some(existing) = state.get_project(&request.project_id) {
        // 如果已存在记录，更新容器信息
        let mut info = (*existing).clone();
        info.set_container(Some(container_info.clone()));
        info
    } else {
        // 如果不存在记录，创建新记录
        let mut info = ProjectAndContainerInfo::new(request.project_id.clone());
        // 入口尽可能记录完整信息（user_id 对两类业务都记录）；
        // 是否参与 user_id 查找由 service_type 在使用方区分（见 adapter 索引门控与 find_projects_by_user_id）。
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
    debug!(
        " [POD_ENSURE] project record updated: project_id={}, user_id={}, container_id={}",
        request.project_id, request.user_id, container_info.container_id
    );

    // 6. 构建响应
    let pod_container_info = PodContainerInfo {
        container_id: container_info.container_id.clone(),
        status: container_info.status.clone(),
    };

    let message = if created {
        "Container created successfully, can access virtual desktop via VNC (Agent service not started)".to_string()
    } else {
        "Container already exists, can access virtual desktop via VNC directly".to_string()
    };

    let response = EnsurePodResponse {
        created,
        container_info: pod_container_info,
        message,
    };

    Ok(HttpResult::success(response))
}
