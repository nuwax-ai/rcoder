//! GetStatus / GetContainerStatus / GetVncStatus RPC 实现

use std::sync::Arc;
use shared_types::grpc::{
    GetContainerStatusRequest, GetContainerStatusResponse, GetStatusRequest, GetStatusResponse,
    GetVncStatusRequest, GetVncStatusResponse,
};
use tonic::{Request, Response, Status};
use tracing::{debug, info, instrument, warn};

use crate::model::AgentStatus;
use crate::router::AppState;
use crate::service::AGENT_REGISTRY;

use super::locale::{locale_from_grpc_request, localized};
use super::utils::check_port_available;

#[instrument(skip(_app_state, request))]
pub async fn get_status(
    _app_state: &Arc<AppState>,
    request: Request<GetStatusRequest>,
) -> Result<Response<GetStatusResponse>, Status> {
    let locale = locale_from_grpc_request(&request);
    shared_types::scope_request_locale(locale, async move {
        let req = request.into_inner();
        info!(
            "📊 [gRPC] GetStatus: project_id={}, session_id={}",
            req.project_id, req.session_id
        );

        let project_id = if !req.session_id.is_empty() {
            AGENT_REGISTRY.get_project_by_session(&req.session_id)
        } else if !req.project_id.is_empty() {
            Some(req.project_id)
        } else {
            info!("📊 [gRPC] GetStatus: all parameters are empty, returning not_found");
            return Ok(Response::new(GetStatusResponse {
                status: "idle".to_string(),
                is_found: false,
            }));
        };

        let (status_str, is_found) = if let Some(ref pid) = project_id {
            if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(pid) {
                let status_str = match agent_info.status {
                    AgentStatus::Pending => "busy",
                    AgentStatus::Active => "busy",
                    AgentStatus::Idle => "idle",
                    AgentStatus::Terminating => "busy",
                };
                (status_str, true)
            } else {
                ("idle", false)
            }
        } else {
            ("idle", false)
        };

        info!(
            "📊 [gRPC] GetStatus result: status={}, is_found={}, project_id={:?}",
            status_str, is_found, project_id
        );

        Ok(Response::new(GetStatusResponse {
            status: status_str.to_string(),
            is_found,
        }))
    })
    .await
}

#[instrument(skip(_app_state))]
pub async fn get_container_status(
    _app_state: &Arc<AppState>,
    request: Request<GetContainerStatusRequest>,
) -> Result<Response<GetContainerStatusResponse>, Status> {
    let locale = locale_from_grpc_request(&request);
    shared_types::scope_request_locale(locale, async move {
        let req = request.into_inner();

        info!(
            "🔍 [GET_CONTAINER_STATUS] Received container status query: user_id={}, project_id={}",
            req.user_id, req.project_id
        );

        let active_tasks = get_active_tasks_count();
        let uptime_seconds = get_uptime_seconds();
        let is_active = active_tasks > 0;

        let status = if active_tasks > 0 {
            "Processing".to_string()
        } else {
            "Idle".to_string()
        };

        let response = GetContainerStatusResponse {
            is_active,
            active_tasks,
            uptime_seconds,
            status: status.clone(),
            cpu_percent: None,
            memory_mb: None,
        };

        debug!(
            "✅ [GET_CONTAINER_STATUS] Returning container status: is_active={}, active_tasks={}, status={}, uptime={}s",
            response.is_active, response.active_tasks, response.status, response.uptime_seconds
        );

        Ok(Response::new(response))
    })
    .await
}

#[instrument(skip(app_state))]
pub async fn get_vnc_status(
    app_state: &Arc<AppState>,
    request: Request<GetVncStatusRequest>,
) -> Result<Response<GetVncStatusResponse>, Status> {
    let locale = locale_from_grpc_request(&request);
    shared_types::scope_request_locale(locale, async move {
        let req = request.into_inner();

        info!(
            "🖥️ [GET_VNC_STATUS] Received VNC status query: user_id={:?}, project_id={:?}",
            req.user_id, req.project_id
        );

        let vnc_ready_file = std::path::Path::new("/tmp/vnc_ready");
        let file_exists = vnc_ready_file.exists();

        let port_check_timeout = app_state
            .config
            .grpc_timeouts
            .as_ref()
            .map(|t| t.port_check_timeout_millis)
            .unwrap_or(500);
        let novnc_port_ready = check_port_available(6080, port_check_timeout).await;

        let vnc_ready = file_exists;
        let novnc_ready = file_exists && novnc_port_ready;

        let message = if vnc_ready && novnc_ready {
            localized(locale, "VNC 服务已就绪", "VNC 服務已就緒", "VNC service is ready")
        } else if file_exists && !novnc_port_ready {
            localized(
                locale,
                "VNC 标记存在，但 noVNC 端口 6080 不可达",
                "VNC 標記存在，但 noVNC 埠 6080 無法連線",
                "VNC marker exists, but noVNC port 6080 is unreachable",
            )
        } else {
            localized(
                locale,
                "VNC 服务未就绪（启动中或启动失败）",
                "VNC 服務尚未就緒（啟動中或啟動失敗）",
                "VNC service is not ready (starting or failed)",
            )
        };

        let uptime_seconds = get_uptime_seconds();

        let response = GetVncStatusResponse {
            vnc_ready,
            novnc_ready,
            message: message.clone(),
            uptime_seconds,
        };

        info!(
            "✅ [GET_VNC_STATUS] Returning status: vnc_ready={}, novnc_ready={}, message={}, uptime={}s",
            response.vnc_ready, response.novnc_ready, response.message, response.uptime_seconds
        );

        Ok(Response::new(response))
    })
    .await
}

fn get_active_tasks_count() -> i32 {
    let count = AGENT_REGISTRY
        .iter_agents()
        .filter(|entry| entry.value().status == AgentStatus::Active)
        .count();

    count as i32
}

fn get_uptime_seconds() -> i64 {
    use std::time::SystemTime;

    static START_TIME: std::sync::OnceLock<SystemTime> = std::sync::OnceLock::new();

    let start = START_TIME.get_or_init(SystemTime::now);

    match SystemTime::now().duration_since(*start) {
        Ok(duration) => duration.as_secs() as i64,
        Err(_) => {
            warn!("[GET_CONTAINER_STATUS] failed to calculate uptime, returning 0");
            0
        }
    }
}
