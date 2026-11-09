//! Computer 容器(整个 pod)资源用量查询。
//!
//! 与 `/computer/agent/status` 互补:该接口返回 agent 进程组粒度,本接口
//! 返回容器 cgroup v2 全量(agent/VNC/桌面全部进程)+ 创建时设定的限额。
//! 数据源同为容器内 agent_runner 的 GetContainerStatus(容器级统计与执行
//! 换代无关,不绑定 expected 身份)。

use axum::extract::State;
use std::sync::Arc;
use tracing::{error, info, instrument, warn};

use crate::router::AppState;
use crate::{AppError, HttpResult};
use shared_types::{ComputerPodResourcesQuery, ComputerPodResourcesResponse};

use super::I18nQuery;

/// 查询 Computer 容器(整个 pod)资源用量
///
/// 返回容器 cgroup v2 的 CPU/内存实时用量与创建时设定的限额。
#[utoipa::path(
    get,
    path = "/computer/pod/resources",
    params(ComputerPodResourcesQuery),
    responses(
        (
            status = 200,
            description = "成功返回容器资源用量(降级场景资源字段缺省,不报错)",
            body = HttpResult<ComputerPodResourcesResponse>,
            example = json!({
                "success": true,
                "data": {
                    "user_id": "user_123",
                    "alive": true,
                    "container_cpu_percent": 35.0,
                    "container_memory_mb": 1080,
                    "container_memory_limit_mb": 3954,
                    "container_cpu_limit_cores": 2.0,
                    "uptime_seconds": 3600,
                    "container_id": "abc123def456"
                },
                "error": null
            })
        ),
        (
            status = 400,
            description = "请求参数错误(user_id/pod_id 均缺省)",
            body = HttpResult<String>
        ),
        (
            status = 404,
            description = "找不到指定的容器",
            body = HttpResult<String>
        ),
        (
            status = 500,
            description = "服务器内部错误",
            body = HttpResult<String>
        )
    ),
    tag = "pod",
    operation_id = "computer_pod_resources",
    summary = "查询 Computer 容器资源用量(整个 pod)",
    description = "按 user_id 或 pod_id 定位 Computer 容器,返回容器 cgroup v2 级别的 CPU/内存实时用量与创建时设定的资源限额。与 /computer/agent/status(agent 进程组粒度)互补:容器口径包含 VNC/桌面环境等全部进程。资源字段在采集降级时缺省(None),不报错。"
)]
#[instrument(skip(state))]
pub async fn computer_pod_resources(
    State(state): State<Arc<AppState>>,
    I18nQuery(params): I18nQuery<ComputerPodResourcesQuery>,
) -> Result<HttpResult<ComputerPodResourcesResponse>, AppError> {
    let locale = shared_types::current_request_locale();

    // 1. 验证参数:user_id 或 pod_id 至少一个
    let has_user_id = params
        .user_id
        .as_ref()
        .is_some_and(|s| !s.trim().is_empty());
    let has_pod_id = params.pod_id.as_ref().is_some_and(|s| !s.trim().is_empty());
    if !has_user_id && !has_pod_id {
        error!("[POD_RESOURCES] user_id or pod_id is required");
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_VALIDATION,
            locale,
        ));
    }

    // 2. 定位容器:user_id 直查,pod_id 按 ComputerAgentRunner 标识符查
    let container_info = if has_user_id {
        crate::service::ComputerContainerManager::get_container_info(
            params.user_id.as_deref().expect("user_id checked above"),
            state.runtime(),
        )
        .await?
    } else {
        state
            .runtime()
            .get_container_info_by_identifier(
                params.pod_id.as_deref().expect("pod_id checked above"),
                &shared_types::ServiceType::ComputerAgentRunner,
            )
            .await
            .map_err(|e| {
                error!("[POD_RESOURCES] container lookup failed: {e}");
                AppError::internal_server_error(&format!("Failed to query container info: {e}"))
            })?
    };

    let Some(container_info) = container_info else {
        warn!(
            "[POD_RESOURCES] Container not found: user_id={:?}, pod_id={:?}",
            params.user_id, params.pod_id
        );
        return Ok(HttpResult::error_with_locale(
            shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
            locale,
        ));
    };

    // 3. 容器未运行:存活信息照实返回,资源字段缺省
    if !super::is_container_running(&container_info.status) {
        info!(
            "[POD_RESOURCES] Container not running: container_id={}, status={}",
            container_info.container_id, container_info.status
        );
        return Ok(HttpResult::success(ComputerPodResourcesResponse {
            user_id: params.user_id.clone(),
            pod_id: params.pod_id.clone(),
            alive: false,
            container_cpu_percent: None,
            container_memory_mb: None,
            container_memory_limit_mb: None,
            container_cpu_limit_cores: None,
            uptime_seconds: 0,
            container_id: Some(container_info.container_id),
            container_name: Some(container_info.container_name),
        }));
    }

    info!(
        "📦 [POD_RESOURCES] Container running: container_id={}, ip={}",
        container_info.container_id, container_info.container_ip
    );

    // 4. gRPC 查询(容器级统计与执行换代无关,不传 expected_session_id;
    //    project_id 传容器归属 project——容器采样不使用该值,仅满足 proto 必填)
    let grpc_addr = shared_types::build_grpc_addr(
        &container_info.container_name,
        &container_info.container_ip,
        &state.config.app_manager.namespace,
        &state.cluster_domain,
    )
    .map_err(|e| {
        error!("[POD_RESOURCES] failed to build grpc addr: {e}");
        AppError::internal_server_error(&format!("Failed to build grpc address: {e}"))
    })?;

    let response = match crate::grpc::status_query::query_container_status(
        &state.grpc_pool,
        &grpc_addr,
        params.user_id.as_deref().unwrap_or_default(),
        &container_info.project_id,
        None,
        std::time::Duration::from_secs(shared_types::GRPC_CONTAINER_STATUS_TIMEOUT_SECS),
    )
    .await
    {
        Ok(response) => response,
        Err(e) => {
            // 降级:容器存活但资源不可得,字段缺省不报错(降级契约)。
            warn!(
                "[POD_RESOURCES] resource query degraded: container_id={}, error={e}",
                container_info.container_id
            );
            return Ok(HttpResult::success(ComputerPodResourcesResponse {
                user_id: params.user_id.clone(),
                pod_id: params.pod_id.clone(),
                alive: true,
                container_cpu_percent: None,
                container_memory_mb: None,
                container_memory_limit_mb: None,
                container_cpu_limit_cores: None,
                uptime_seconds: 0,
                container_id: Some(container_info.container_id),
                container_name: Some(container_info.container_name),
            }));
        }
    };

    info!(
        "✅ [POD_RESOURCES] container resources: container_id={}, cpu={:?}, mem={:?}MB",
        container_info.container_id, response.container_cpu_percent, response.container_memory_mb
    );

    Ok(HttpResult::success(ComputerPodResourcesResponse {
        user_id: params.user_id.clone(),
        pod_id: params.pod_id.clone(),
        alive: true,
        container_cpu_percent: response.container_cpu_percent,
        container_memory_mb: response.container_memory_mb,
        container_memory_limit_mb: response.container_memory_limit_mb,
        container_cpu_limit_cores: response.container_cpu_limit_cores,
        uptime_seconds: response.uptime_seconds,
        container_id: Some(container_info.container_id),
        container_name: Some(container_info.container_name),
    }))
}
