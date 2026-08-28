//! pod_ensure 主流程的阶段实现（从 ensure.rs 拆出，控制流/日志逐条保持）。
//!
//! [`wait_for_concurrent_creation`]（并发创建等待）→ [`resolve_need_create`]
//! （存在性判定与旧容器清理）→ [`create_with_retry`] / [`get_existing_with_sync`]
//! （创建或复用）→ 主流程尾部的注册/存储/响应用 [`super::helpers::persist_and_respond`]。

use super::helpers::*;
use super::*;

/// 等待并发创建方完成：创建标记存在且未过期时订阅 broadcast 等待（30s 上限）。
///
/// 返回 `Some(response)` = 等到容器就绪，直接以复用语义短路返回；
/// `None` = 无并发创建 / 标记过期已清理 / 等待超时，继续正常创建流程。
///
/// 🚀 先订阅 broadcast channel 再检查 pod_creating——避免 subscribe-after-send
/// 竞态：如果在检查 pod_creating 之后才订阅，创建者可能已经移除了标记并发送了
/// 通知，导致我们错过消息。
pub(super) async fn wait_for_concurrent_creation(
    state: &Arc<AppState>,
    request: &EnsurePodRequest,
    service_type: &ServiceType,
    container_identifier: &str,
) -> Option<Result<HttpResult<EnsurePodResponse>, AppError>> {
    let mut rx = state.pod_created_tx.subscribe();

    // view() 在闭包返回后立即释放锁，无 Ref 暴露
    if let Some(elapsed) = state
        .pod_creating
        .view(container_identifier, |_, t| t.elapsed())
    {
        // 标记超过 60 秒视为过期（创建方可能已崩溃），忽略并继续
        if elapsed < std::time::Duration::from_secs(60) {
            info!(
                "[POD_ENSURE] Container is being created, waiting for completion: container_identifier={}, elapsed={:?}",
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
                            if !state.pod_creating.contains_key(container_identifier) {
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
                        .get_container_info_by_identifier(container_identifier, service_type)
                        .await
                    {
                        info!(
                            "[POD_ENSURE] Wait succeeded, container ready: container_identifier={}, container_id={}",
                            container_identifier, info.container_id
                        );
                        waited_container_info = Some(info);
                    }
                }
                Err(_) => {
                    // 超时处理
                    warn!(
                        "[POD_ENSURE] Wait for container creation timeout (30s): container_identifier={}",
                        container_identifier
                    );
                }
            }

            // 如果等待成功，直接使用已就绪的容器（由并发请求创建），跳过创建流程
            if let Some(info) = waited_container_info {
                return Some(persist_and_respond(
                    state,
                    request,
                    service_type,
                    &info,
                    false,
                    format!(
                        "Container ready (waiting for other request to complete creation): container_id={}",
                        info.container_id
                    ),
                ));
            }
            // 等待超时，继续正常的创建流程（此时标记可能已过期被清理）
            warn!(
                "[POD_ENSURE] Wait for container creation timeout (30s), will continue to try creating: container_identifier={}",
                container_identifier
            );
        } else {
            // 标记过期，清理后继续
            warn!(
                "[POD_ENSURE] Creation mark expired ({:?}), cleaning up and continuing",
                elapsed
            );
            state.pod_creating.remove(container_identifier);
        }
    }
    None
}

/// 实时查询 runtime 判定是否需要创建；容器存在但未运行时删除旧容器并清理
/// 其 SSE 共享流与 gRPC 连接（post-destroy），为重建铺路。
pub(super) async fn resolve_need_create(
    state: &Arc<AppState>,
    container_identifier: &str,
    service_type: &ServiceType,
) -> Result<bool, AppError> {
    let runtime = state.runtime().clone();

    let existing_container = runtime
        .find_container(container_identifier, service_type)
        .await
        .map_err(|e| {
            error!("[POD_ENSURE] Failed to query container status: {}", e);
            AppError::internal_server_error(&format!("Failed to query container status: {}", e))
        })?;

    match existing_container {
        Some(result) if result.status == container_runtime_api::ContainerRuntimeStatus::Running => {
            // 容器存在且正在运行，无需创建
            info!(
                "[POD_ENSURE] Container already exists and running: container_id={}, status={:?}",
                result.container_id, result.status
            );
            Ok(false)
        }
        Some(result) => {
            // 容器存在但未运行（Exited 等状态），需要删除并重建
            warn!(
                "[POD_ENSURE] Container exists but not running: container_id={}, status={:?}, will delete and recreate",
                result.container_id, result.status
            );

            // 删除旧容器（使用 pod_id 优先的标识符，与创建时一致）
            // 如果删除失败（包括容器不存在等情况），返回错误让调用者知道
            runtime
                .stop_container_by_identifier(container_identifier, service_type)
                .await
                .map_err(|e| {
                    error!(
                        "[POD_ENSURE] Failed to delete old container: container_id={}, error={}",
                        result.container_id, e
                    );
                    AppError::internal_server_error(&format!(
                        "Failed to delete old container: {}",
                        e
                    ))
                })?;

            info!(
                "[POD_ENSURE] Old container deleted: container_id={}",
                result.container_id
            );

            // 物理销毁成功后，关闭旧容器的 SSE 共享流 + 清理 gRPC 连接（post-destroy：
            // stop 用 ? 返回，失败时此处不执行，避免误断可能仍存活的容器连接）。
            // 地址走 build_grpc_addr（K8s Service FQDN / Docker 容器 IP，与连接建立同源）。
            if shared_types::is_kubernetes_runtime() || !result.container_ip.is_empty() {
                let old_grpc_addr = shared_types::build_grpc_addr(
                    &result.container_name,
                    &result.container_ip,
                    &state.config.app_manager.namespace,
                    &state.cluster_domain,
                );
                state.shutdown_sse_streams_by_addr(&old_grpc_addr);
                state.grpc_pool.remove(&old_grpc_addr).await;
            }

            // ⏱️ 等待 Docker 完全释放容器资源（避免竞态条件）
            // Docker 删除是异步操作，立即创建同名容器可能导致资源冲突
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            debug!("[POD_ENSURE] container resources already released");

            Ok(true)
        }
        None => {
            // 容器不存在，需要创建
            info!("[POD_ENSURE] container not found, will create new container");
            Ok(true)
        }
    }
}

/// 设置创建标记并以 3 次指数退避重试创建容器；成功/失败均清除标记并
/// broadcast 通知等待方（无等待者时记 warn——pod 创建重，可能白创建）。
pub(super) async fn create_with_retry(
    state: &Arc<AppState>,
    request: &EnsurePodRequest,
    service_type: &ServiceType,
    container_identifier: &str,
) -> Result<ContainerBasicInfo, AppError> {
    // 🆕 设置创建标记，防止并发请求重复创建
    let create_started = Instant::now();
    state
        .pod_creating
        .insert(container_identifier.to_string(), create_started);

    info!(
        "[POD_ENSURE] Creation marker set: container_identifier={}, user_id={}, project_id={}, max_attempts=3",
        container_identifier, request.user_id, request.project_id
    );

    // 创建新容器，最多重试 3 次
    let resource_limits =
        resolve_resource_limits_from_config(state, service_type, request.resource_limits.clone())?;

    let mut last_error = None;
    let mut result = None;
    let max_attempts = 3;

    for attempt in 1..=max_attempts {
        let attempt_started = Instant::now();
        info!(
            "[POD_ENSURE] Container creation attempt {}/{} started: container_identifier={}, elapsed_since_marker={:?}",
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
                    "[POD_ENSURE] Container created successfully (attempt {}): container_id={}, ip={}, attempt_elapsed={:?}, total_elapsed={:?}",
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
                        "[POD_ENSURE] Container creation failed (attempt {}/{}), will retry: error={}, attempt_elapsed={:?}, total_elapsed={:?}",
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
                    tokio::time::sleep(tokio::time::Duration::from_millis(200 * attempt as u64))
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
                "[POD_ENSURE] Clearing creation marker after success: container_identifier={}, total_elapsed={:?}",
                container_identifier,
                create_started.elapsed()
            );
            // 创建成功，清除标记
            state.pod_creating.remove(container_identifier);
            // 🚀 发送容器创建完成通知（唤醒等待方）；无等待者时记 warn（pod 创建重，可能白创建）
            if let Err(send_err) = state.pod_created_tx.send(container_identifier.to_string()) {
                tracing::warn!("pod_created notify failed (no waiter subscribed): {send_err}");
            }
            Ok(info)
        }
        None => {
            debug!(
                "[POD_ENSURE] Clearing creation marker after failure: container_identifier={}, total_elapsed={:?}",
                container_identifier,
                create_started.elapsed()
            );
            // 创建失败，也要清除标记
            state.pod_creating.remove(container_identifier);
            // 直接返回原始错误，保留具体的错误信息
            Err(last_error.unwrap_or_else(|| {
                AppError::internal_server_error(
                    "Container creation failed but no error info captured",
                )
            }))
        }
    }
}

/// 获取现有运行容器的完整信息；内部映射未同步时短暂等待（3 次 1s），
/// 仍失败才走兜底重建（同样设置/清除创建标记并 broadcast 通知）。
pub(super) async fn get_existing_with_sync(
    state: &Arc<AppState>,
    request: &EnsurePodRequest,
    service_type: &ServiceType,
    container_identifier: &str,
) -> Result<(ContainerBasicInfo, bool), AppError> {
    let runtime = state.runtime().clone();

    match runtime
        .get_container_info_by_identifier(container_identifier, service_type)
        .await
    {
        Ok(Some(info)) => {
            // 容器信息正常获取
            Ok((info, false))
        }
        Ok(None) => {
            // Docker API 确认容器在运行，但内部 map 还没同步
            // 短暂等待让内部 map 同步，而不是直接重建
            warn!(
                "[POD_ENSURE] Container running but internal mapping not ready, waiting for sync: container_identifier={}",
                container_identifier
            );

            let mut retry_info = None;
            for retry_attempt in 1..=3 {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                match runtime
                    .get_container_info_by_identifier(container_identifier, service_type)
                    .await
                {
                    Ok(Some(info)) => {
                        info!(
                            "[POD_ENSURE] Internal mapping synced (retry {}): container_id={}",
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
                Some(info) => Ok((info, false)),
                None => {
                    // 3次重试后仍失败，才考虑重建
                    warn!(
                        "[POD_ENSURE] Wait for sync timeout, attempting to recreate: container_identifier={}",
                        container_identifier
                    );

                    let resource_limits = resolve_resource_limits_from_config(
                        state,
                        service_type,
                        request.resource_limits.clone(),
                    )?;

                    // 设置创建标记
                    state
                        .pod_creating
                        .insert(container_identifier.to_string(), Instant::now());

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
                    state.pod_creating.remove(container_identifier);

                    // 🚀 发送容器创建完成通知（唤醒等待方）；无等待者时记 warn（pod 创建重，可能白创建）
                    if result.is_ok()
                        && let Err(send_err) =
                            state.pod_created_tx.send(container_identifier.to_string())
                    {
                        tracing::warn!(
                            "pod_created notify failed (no waiter subscribed): {send_err}"
                        );
                    }

                    match result {
                        Ok(info) => {
                            info!(
                                "[POD_ENSURE] Container recreated successfully: container_id={}",
                                info.container_id
                            );
                            Ok((info, true))
                        }
                        Err(e) => {
                            error!(
                                "[POD_ENSURE] Container recreation failed: container_identifier={}, error={}",
                                container_identifier, e
                            );
                            Err(e)
                        }
                    }
                }
            }
        }
        Err(e) => {
            error!(
                "[POD_ENSURE] Failed to get container full info: container_identifier={}, error={}",
                container_identifier, e
            );
            Err(AppError::internal_server_error(&format!(
                "Failed to get container full info: {}",
                e
            )))
        }
    }
}
