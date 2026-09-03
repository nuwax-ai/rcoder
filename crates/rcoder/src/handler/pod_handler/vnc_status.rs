//! pod VNC 状态端点（自 status.rs 按端点轴拆出；函数体原样搬迁）。

use super::helpers::*;
use super::*;
use shared_types::ProjectStore as _; // 存储契约 trait：state.projects（ProjectStoreBackend）方法经此解析

/// 查询容器 VNC 服务状态
///
/// 根据 user_id 或 project_id 定位容器，查询 VNC/noVNC 服务是否已启动就绪。
#[utoipa::path(
    get,
    path = "/computer/pod/vnc-status",
    params(VncStatusQuery),
    responses(
        (status = 200, description = "成功获取 VNC 状态", body = HttpResult<VncStatusResponse>),
        (status = 400, description = "参数无效", body = HttpResult<String>),
        (status = 401, description = "API Key 鉴权失败", body = HttpResult<String>),
        (status = 404, description = "容器不存在", body = HttpResult<String>),
        (status = 500, description = "服务器内部错误", body = HttpResult<String>)
    ),
    tag = "pod",
    operation_id = "pod_vnc_status",
    summary = "查询容器 VNC 服务状态",
    description = "根据 user_id 或 project_id 定位子容器，查询 VNC/noVNC 服务是否已启动就绪"
)]
#[instrument(skip(state), fields(app_id = ?params.app_id))]
pub async fn pod_vnc_status(
    State(state): State<Arc<AppState>>,
    I18nQuery(params): I18nQuery<VncStatusQuery>,
) -> Result<HttpResult<VncStatusResponse>, AppError> {
    let locale = shared_types::current_request_locale();

    // 0. userApp 分派（app_id 存在即短路 agent 流程；service_type=userapp 搭配放行）
    match parse_app_target(
        params.app_id.as_deref(),
        params.app_stage.as_deref(),
        params.service_type.as_deref(),
    ) {
        Ok(AppTarget::NotApp) => {}
        Ok(AppTarget::Dev(app_id)) => return vnc_status_userapp_dev(&state, locale, app_id).await,
        Ok(AppTarget::Prod(app_id)) => {
            // 生产容器跑用户业务应用（无 VNC/noVNC、无 agent_runner 可供 gRPC 探测），
            // 恒报 not-ready（200）——调用方统一轮询逻辑无需分叉处理错误分支。
            info!("[POD_VNC_STATUS] userapp prod container has no VNC: app_id={app_id}");
            return Ok(HttpResult::success(VncStatusResponse {
                vnc_ready: false,
                novnc_ready: false,
                message: "VNC is not available for userApp prod containers".to_string(),
                uptime_seconds: Some(0),
                container_id: None,
            }));
        }
        Err(e) => {
            error!("[POD_VNC_STATUS] invalid app target: {}", e);
            return Err(AppError::with_message(
                shared_types::error_codes::ERR_VALIDATION,
                e,
            ));
        }
    }

    // 1. 参数验证：pod_id、user_id 和 project_id 不能同时为空
    let user_id = params.user_id.as_deref().filter(|s| !s.trim().is_empty());
    let project_id = params
        .project_id
        .as_deref()
        .filter(|s| !s.trim().is_empty());
    let pod_id = params.pod_id.as_deref().filter(|s| !s.trim().is_empty());

    // 1.1 解析 service_type
    let service_type = match parse_service_type(params.service_type.as_deref()) {
        Ok(st) => st,
        Err(e) => {
            error!("[POD_VNC_STATUS] invalid service_type: {}", e);
            return Err(AppError::with_message(
                shared_types::error_codes::ERR_VALIDATION,
                e.to_string(),
            ));
        }
    };

    // 1.2 验证隔离参数完整性（当 pod_id 有值时）
    if pod_id.is_some()
        && (params.isolation_type.is_none()
            || params.tenant_id.is_none()
            || params.space_id.is_none())
    {
        error!(
            "[POD_VNC_STATUS] Validation failed: isolation_type, tenant_id, space_id are required when pod_id is provided"
        );
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            "isolation_type, tenant_id, space_id are all required when pod_id is provided",
        ));
    }

    if pod_id.is_none() && user_id.is_none() && project_id.is_none() {
        warn!("[POD_VNC_STATUS] pod_id, user_id and project_id are all empty");
        return Err(AppError::with_message(
            shared_types::error_codes::ERR_VALIDATION,
            "at least one of pod_id, user_id or project_id is required",
        ));
    }

    info!(
        "[POD_VNC_STATUS] Querying VNC status: user_id={:?}, project_id={:?}, pod_id={:?}",
        user_id, project_id, pod_id
    );

    // 2. 获取 Runtime
    let runtime = state.runtime().clone();

    // 3. 定位容器
    // 优先级：pod_id > user_id > project_id
    let (_lookup_user_id, container_info) = if let Some(pid) = pod_id {
        // 使用 pod_id 查找（多租户场景）
        (pid, runtime.find_container(pid, &service_type).await)
    } else if let Some(uid) = user_id {
        (uid, runtime.find_container(uid, &service_type).await)
    } else if let Some(pid) = project_id {
        // 如果只有 project_id，通过 storage lookup 关联的容器
        if state
            .projects
            .get_container_by_user_id(pid, &service_type)
            .is_some()
        {
            // project_id 可能实际上是 user_id
            (pid, runtime.find_container(pid, &service_type).await)
        } else {
            (pid, Ok(None))
        }
    } else {
        ("", Ok(None))
    };

    let container_info = container_info.map_err(|e| {
        error!("[POD_VNC_STATUS] Failed to query container: {}", e);
        AppError::internal_server_error(&format!("Failed to query container: {}", e))
    })?;

    // 4. 检查容器是否存在
    let result = match container_info {
        Some(info) => info,
        None => {
            info!(
                "[POD_VNC_STATUS] Container does not exist: user_id={:?}, project_id={:?}",
                user_id, project_id
            );
            return Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
                locale,
            ));
        }
    };

    // 5. 检查容器是否正在运行
    if result.status != container_runtime_api::ContainerRuntimeStatus::Running {
        info!(
            "[POD_VNC_STATUS] Container not running: container_id={}",
            result.container_id
        );
        return Ok(HttpResult::success(vnc_not_running(result.container_id)));
    }

    // 5.1 🎯 确保 VNC 代理路由已注册
    // 解决竞态条件：VNC 服务已就绪，但代理路由尚未注册
    // 在 handle_computer_chat 时会注册路由，但 VNC 状态检查可能在 chat 之前调用
    if let Some(ref pingora_service) = state.pingora_service
        && let Some(uid) = user_id
    {
        pingora_service.add_vnc_backend(uid, &result.container_ip);
        debug!(
            "🔗 [POD_VNC_STATUS] Ensured VNC backend registered: user_id={} -> {}",
            uid, result.container_ip
        );
    }

    // 6. gRPC 真查 VNC 探针（与 userapp dev 路径共享链路）
    probe_vnc_via_grpc(
        &state,
        locale,
        VncProbeTarget {
            name: &result.container_name,
            ip: &result.container_ip,
            container_id: result.container_id,
        },
        user_id,
        project_id,
        "agent",
    )
    .await
}

/// VNC 状态 gRPC 探测共享段（agent 与 userapp dev 两路径原为 ~37 行逐字重复）：
/// build_grpc_addr → get_client → get_vnc_status → 组装/错误分支。
/// 运行判定与容器定位留在调用点（两路径的 status 类型/定位链不同），
/// 日志上下文经 `log_ctx` 区分（下游按 tag 检索）。
/// 探测目标三元组（agent 路径 RuntimeContainerInfo 与 userapp dev 路径
/// ContainerBasicInfo 类型不同，取共享字段打包）
struct VncProbeTarget<'a> {
    name: &'a str,
    ip: &'a str,
    container_id: String,
}

async fn probe_vnc_via_grpc(
    state: &AppState,
    locale: &'static str,
    target: VncProbeTarget<'_>,
    user_id: Option<&str>,
    project_id: Option<&str>,
    log_ctx: &str,
) -> Result<HttpResult<VncStatusResponse>, AppError> {
    // K8s 用 Service FQDN，Docker 用容器 IP（统一走 shared_types 分发）
    let grpc_addr = shared_types::build_grpc_addr(
        target.name,
        target.ip,
        &state.config.app_manager.namespace,
        &state.cluster_domain,
    );

    match state.grpc_pool.get_client(&grpc_addr).await {
        Ok(mut client) => {
            let grpc_request = crate::grpc::new_request_with_locale(
                shared_types::grpc::GetVncStatusRequest {
                    user_id: user_id.map(String::from),
                    project_id: project_id.map(String::from),
                },
                locale,
            );

            match client.get_vnc_status(grpc_request).await {
                Ok(response) => {
                    let resp = response.into_inner();
                    info!(
                        "[POD_VNC_STATUS] {log_ctx} gRPC ok: vnc_ready={}, novnc_ready={}",
                        resp.vnc_ready, resp.novnc_ready
                    );

                    Ok(HttpResult::success(VncStatusResponse {
                        vnc_ready: resp.vnc_ready,
                        novnc_ready: resp.novnc_ready,
                        message: resp.message,
                        uptime_seconds: Some(resp.uptime_seconds),
                        container_id: Some(target.container_id),
                    }))
                }
                Err(e) => {
                    error!("[POD_VNC_STATUS] {log_ctx} gRPC call failed: {e}");
                    Ok(HttpResult::error_with_locale(
                        shared_types::error_codes::ERR_GRPC_ERROR,
                        locale,
                    ))
                }
            }
        }
        Err(e) => {
            error!("[POD_VNC_STATUS] {log_ctx} gRPC connection failed: {e}");
            Ok(HttpResult::error_with_locale(
                shared_types::error_codes::ERR_GRPC_ERROR,
                locale,
            ))
        }
    }
}

/// 容器未运行时的 VNC 状态响应（agent 与 userapp dev 两路径逐字同构）
fn vnc_not_running(container_id: String) -> VncStatusResponse {
    VncStatusResponse {
        vnc_ready: false,
        novnc_ready: false,
        message: "Container not running".to_string(),
        uptime_seconds: Some(0),
        container_id: Some(container_id),
    }
}

/// vnc-status 的 userApp dev 分支：开发容器（UserappBuilder）复用 agent-runner
/// 镜像，VNC 栈（Xvnc 5900 + noVNC 6080）实际在跑（桌面入口
/// `/api/v1/userapp/proxy/vnc/dev/{app_id}`）——走既有 gRPC 链路真查容器内探针。
/// 不注册 `vnc_backends`：那是 computer 域 user_id 键空间，userApp VNC 走独立
/// app_id 路由（ContainerLookup 动态解析）。
async fn vnc_status_userapp_dev(
    state: &Arc<AppState>,
    locale: &'static str,
    app_id: String,
) -> Result<HttpResult<VncStatusResponse>, AppError> {
    let existing = state
        .runtime()
        .get_container_info_by_identifier(&app_id, &ServiceType::UserappBuilder)
        .await
        .map_err(|e| {
            error!("[POD_VNC_STATUS] userapp dev container lookup failed: app_id={app_id}: {e:#}");
            AppError::internal_server_error(&format!("userapp dev container lookup failed: {e:#}"))
        })?;
    let result = match existing {
        Some(info) => info,
        None => {
            info!("[POD_VNC_STATUS] userapp dev container does not exist: app_id={app_id}");
            return Ok(HttpResult::error_with_message(
                shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
                locale,
                &format!("userapp dev container not found: app_id={app_id}"),
            ));
        }
    };
    if !is_container_running(&result.status) {
        info!(
            "[POD_VNC_STATUS] userapp dev container not running: container_id={}",
            result.container_id
        );
        return Ok(HttpResult::success(vnc_not_running(result.container_id)));
    }

    // gRPC 真查（与 agent 路径同链路：K8s 用 Service FQDN，Docker 用容器 IP）
    probe_vnc_via_grpc(
        state,
        locale,
        VncProbeTarget {
            name: &result.container_name,
            ip: &result.container_ip,
            container_id: result.container_id,
        },
        None,
        Some(&app_id),
        "userapp dev",
    )
    .await
}
