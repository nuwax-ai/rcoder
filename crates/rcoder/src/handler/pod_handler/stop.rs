use super::helpers::*;
use super::*;

/// 停止并销毁容器（保留数据卷）
///
/// 根据 user_id / project_id / service_type 定位容器并销毁。
/// 物理销毁走 runtime 层：K8s 删 STS + Service（PVC 保留，下次 ensure 重建挂回），
/// Docker 删容器。对话状态在 agent 内存，停止即断会话。
/// 容器不存在时幂等返回成功（was_existing=false）。
#[utoipa::path(
    post,
    path = "/computer/pod/stop",
    request_body(content = StopPodRequest, description = "停止容器请求"),
    responses(
        (status = 200, description = "成功停止容器", body = HttpResult<StopPodResponse>),
        (status = 400, description = "请求参数无效", body = HttpResult<String>),
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_stop",
    summary = "停止并销毁容器（保留数据卷）",
    description = "根据 user_id / project_id / service_type 定位容器并销毁：K8s 删 STS + Service（PVC 保留，数据不丢，下次 ensure 重建挂回），Docker 删容器。携带 app_id 时进入 userApp 分派：dev=销毁 UserAppBuilder 开发容器（per-app PVC 保留）；prod=scale-to-0 停止生产实例（阻断流量唤醒，ensure 可显式唤醒）。容器不存在时幂等返回成功。注意：对话状态在 agent 内存中，停止即断会话。"
)]
#[instrument(skip(state), fields(user_id = %request.user_id, project_id = %request.project_id))]
pub async fn pod_stop(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(request): I18nJsonOrQuery<StopPodRequest>,
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
            return stop_userapp_dev(&state, app_id).await;
        }
        Ok(AppTarget::Prod(app_id)) => return stop_userapp_prod(&state, locale, app_id).await,
        Err(e) => {
            error!("[POD_STOP] invalid app target: {}", e);
            return Ok(invalid_app_target_response(locale, &e));
        }
    }

    // 1. 验证参数
    if request.user_id.trim().is_empty() {
        error!("[POD_STOP] user_id is required");
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "user_id is required and cannot be empty",
        ));
    }
    if request.project_id.trim().is_empty() {
        error!("[POD_STOP] project_id is required");
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
            "project_id is required and cannot be empty",
        ));
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

    // 2. 检查容器是否存在（不存在=幂等空操作）
    let existing_container = ComputerContainerManager::get_container_info_with_type(
        &container_identifier,
        state.runtime(),
        &service_type,
    )
    .await?;
    let Some(container_info) = existing_container else {
        info!(
            "[POD_STOP] Container does not exist (idempotent no-op): container_identifier={}",
            container_identifier
        );
        return Ok(HttpResult::success(StopPodResponse {
            was_existing: false,
            message: "Container did not exist (idempotent no-op)".to_string(),
        }));
    };

    // 3. 物理销毁（K8s：STS+svc，PVC 保留；Docker：删容器）。
    //    与 restart 的 destroy 半程相反，此处先停后清账：物理失败即报错返回，
    //    存储记录未动，调用方可安全重试（stop 幂等）。
    if let Err(e) = state
        .runtime()
        .stop_container_by_identifier(&container_identifier, &service_type)
        .await
    {
        error!(
            "[POD_STOP] Failed to stop container: container_identifier={}, error={}",
            container_identifier, e
        );
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_BACKEND_ERROR,
            format!("failed to stop container {container_identifier}: {e}"),
        ));
    }
    info!(
        "[POD_STOP] Container destroyed: container_id={}",
        container_info.container_id
    );

    // 4. 清理存储记录（容器 + 关联 project 行 + sessions claim 归还）。
    //    物理已销毁，此处失败仅 warn——ensure 的实时查询自愈可兜底死记录。
    let (container_deleted, deleted_projects) = state
        .delete_container_with_projects_durable(&container_info.container_id)
        .await;
    info!(
        "[POD_STOP] Cleaned up container records: container_id={}, container_deleted={}, deleted_projects={}",
        container_info.container_id, container_deleted, deleted_projects
    );

    // 5. 关闭旧容器的 SSE 共享流 + 清理 gRPC 连接
    state
        .teardown_container_connections(
            &container_info.container_name,
            &container_info.container_ip,
        )
        .await;

    // 6. 清理 Pingora backend（stop 不重建，必须显式摘除，防流量打到死地址）。
    //    键与注册侧对齐而非 container_identifier——后者在 pod_id 存在时为 pod_id：
    //    VNC 按 user_id 注册（ensure/restart 的 register_vnc_backend 均用
    //    request.user_id），project 按 project_id 注册（同 destroyer/agent_stop 的
    //    键选择）
    if let Some(ref pingora) = state.pingora_service {
        match service_type {
            ServiceType::ComputerAgentRunner => {
                pingora.remove_vnc_backend(&request.user_id);
            }
            ServiceType::WebAgentRunner => {
                pingora.remove_project_backend(&request.project_id);
            }
            _ => {}
        }
    }

    // 7. 轮询确认容器真正移除（最多 5s；不确认只 warn 不翻案——物理销毁已返回成功）
    let mut deletion_confirmed = false;
    for i in 0..10 {
        match state
            .runtime()
            .find_container(&container_identifier, &service_type)
            .await
        {
            Ok(Some(_)) => {
                if i == 0 {
                    info!(
                        "[POD_STOP] Container still exists, waiting for cleanup: container_identifier={}",
                        container_identifier
                    );
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
            Ok(None) => {
                info!(
                    "[POD_STOP] Confirmed container removed: container_identifier={}",
                    container_identifier
                );
                deletion_confirmed = true;
                break;
            }
            Err(e) => {
                warn!(
                    "[POD_STOP] check container removed status: {}, container already removed",
                    e
                );
                deletion_confirmed = true;
                break;
            }
        }
    }
    if !deletion_confirmed {
        warn!(
            "[POD_STOP] Wait for container removal timeout (physical destroy already succeeded): container_identifier={}",
            container_identifier
        );
    }

    info!(
        "[POD_STOP] Completed: was_existing=true, container_id={}",
        container_info.container_id
    );

    Ok(HttpResult::success(StopPodResponse {
        was_existing: true,
        message: "Container stopped and destroyed (data volume preserved)".to_string(),
    }))
}

// ============================================================================
// userApp 分派实现（app_id/app_stage）
// ============================================================================

/// stop 的 userApp dev 分支：销毁 UserAppBuilder 开发容器（per-app PVC 保留，
/// 数据不丢）。清注册 container 字段而非 remove_project——保 PG 侧 project 行
/// 与会话映射；探活缓存一并失效，防下次 ensure 命中死 IP。
async fn stop_userapp_dev(
    state: &Arc<AppState>,
    app_id: String,
) -> Result<HttpResult<StopPodResponse>, AppError> {
    // 区分查询错误与真不存在（K8s API 瞬断不应误报幂等成功）
    let existed = state
        .runtime()
        .get_container_info_by_identifier(&app_id, &ServiceType::UserAppBuilder)
        .await
        .map_err(|e| {
            error!("[POD_STOP] userapp dev container lookup failed: app_id={app_id}: {e:#}");
            AppError::with_message(
                shared_types::error_codes::ERR_BACKEND_ERROR,
                format!("userapp dev container lookup failed: {e:#}"),
            )
        })?;
    if existed.is_none() {
        info!(
            "[POD_STOP] userapp dev container does not exist (idempotent no-op): app_id={app_id}"
        );
        return Ok(HttpResult::success(StopPodResponse {
            was_existing: false,
            message: "UserApp dev 容器不存在（幂等空操作）".to_string(),
        }));
    }

    // 物理销毁（K8s：STS+svc，per-app PVC 保留；Docker：删容器+卷映射，宿主目录保留）
    state
        .runtime()
        .stop_container_by_identifier(&app_id, &ServiceType::UserAppBuilder)
        .await
        .map_err(|e| {
            error!("[POD_STOP] userapp dev stop failed: app_id={app_id}: {e:#}");
            AppError::with_message(
                shared_types::error_codes::ERR_BACKEND_ERROR,
                format!("userapp dev stop failed: {e:#}"),
            )
        })?;
    info!("[POD_STOP] userapp dev 容器已销毁: app_id={app_id}");

    // 清 SSE 流 + 注册表 container 字段（保 PG project 行与会话映射）
    state.shutdown_sse_streams_for_project(&app_id);
    if let Some(mut info) = state.get_project(&app_id).map(|p| (*p).clone()) {
        info.set_container(None);
        if let Err(e) = state.insert_project(app_id.clone(), Arc::new(info)) {
            warn!("[POD_STOP] clear stale container field failed: app_id={app_id}: {e}");
        }
    }
    crate::userapp_forward::invalidate_probe_cache(&app_id);

    info!("[POD_STOP] userapp dev stop completed: app_id={app_id}");
    Ok(HttpResult::success(StopPodResponse {
        was_existing: true,
        message: "UserApp dev 容器已停止（开发卷保留，数据不丢）".to_string(),
    }))
}

/// stop 的 userApp prod 分支：scale-to-0 停止（阻断流量唤醒；显式 ensure 仍可唤醒）。
async fn stop_userapp_prod(
    state: &Arc<AppState>,
    locale: &str,
    app_id: String,
) -> Result<HttpResult<StopPodResponse>, AppError> {
    // 存在性校验：不存在的 app 幂等语义不适用（app 元数据仍在，需明确报错防幻报）
    if let Err(e) = state.app_service.get_app(&app_id).await {
        error!("[POD_STOP] userapp prod app not found: app_id={app_id}: {e:#}");
        return Ok(HttpResult::error_with_message(
            shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
            locale,
            &format!("userapp prod app not found: app_id={app_id}"),
        ));
    }
    state.app_service.stop_app(&app_id).await.map_err(|e| {
        error!("[POD_STOP] userapp prod stop failed: app_id={app_id}: {e:#}");
        AppError::with_message(
            shared_types::error_codes::ERR_BACKEND_ERROR,
            format!("userapp prod stop failed: {e:#}"),
        )
    })?;
    info!("[POD_STOP] userapp prod 已停止（scale 0）: app_id={app_id}");
    Ok(HttpResult::success(StopPodResponse {
        was_existing: true,
        message: "UserApp 生产实例已停止（流量唤醒已阻断，ensure 可显式唤醒）".to_string(),
    }))
}
