//! GetStatus / GetContainerStatus / GetVncStatus RPC 实现

use shared_types::grpc::{
    GetContainerStatusRequest, GetContainerStatusResponse, GetStatusRequest, GetStatusResponse,
    GetVncStatusRequest, GetVncStatusResponse,
};
use std::sync::Arc;
use tonic::{Request, Response, Status};
use tracing::{debug, info, instrument};

use crate::model::AgentStatus;
use crate::router::AppState;
use crate::service::AGENT_REGISTRY;

use super::locale::locale_from_grpc_request;
use super::utils::probe_vnc_readiness;

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
            info!("[gRPC] GetStatus: all parameters are empty, returning not_found");
            return Ok(Response::new(GetStatusResponse {
                status: "not_found".to_string(),
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
                // project_id 存在但 agent_info 不存在
                info!(
                    "📊 [gRPC] GetStatus: project_id found but agent_info missing, returning not_found: pid={}",
                    pid
                );
                ("not_found", false)
            }
        } else {
            // session_id 在 AGENT_REGISTRY 中找不到
            info!(
                "📊 [gRPC] GetStatus: session_id not found in registry, returning not_found: session_id={}",
                req.session_id
            );
            ("not_found", false)
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

        let port_check_timeout = app_state
            .config
            .grpc_timeouts
            .as_ref()
            .map(|t| t.port_check_timeout_millis)
            .unwrap_or(500);

        // 复用共享探测逻辑（noVNC 前端层 + Xvnc RFB 后端层 + i18n message），
        // 与 HTTP `/computer/agent/vnc-status` 保持 vnc_ready/novnc_ready/message 语义一致。
        let probed = probe_vnc_readiness(port_check_timeout, locale).await;

        let uptime_seconds = get_uptime_seconds();

        let response = GetVncStatusResponse {
            vnc_ready: probed.vnc_ready,
            novnc_ready: probed.novnc_ready,
            message: probed.message.clone(),
            uptime_seconds,
        };

        info!(
            "✅ [GET_VNC_STATUS] Returning status: vnc_ready={}, novnc_ready={}, port_ready={}, websocket_ready={}, rfb_ready={}, message={}, uptime={}s",
            response.vnc_ready,
            response.novnc_ready,
            probed.novnc_port_ready,
            probed.novnc_websocket_ready,
            probed.rfb_ready,
            response.message,
            response.uptime_seconds
        );

        Ok(Response::new(response))
    })
    .await
}

fn get_active_tasks_count() -> i32 {
    let agent_count = AGENT_REGISTRY
        .iter_agents()
        .filter(|entry| entry.value().status == AgentStatus::Active)
        .count() as i32;

    // 终端 WS 连接也算「容器在用」：终端流量本身不计入 agent active task，
    // 这里把它叠加进 active_tasks，使 idle cleaner 的 gRPC 二次确认能拦下「终端在用」的容器。
    // （http-server 关闭时 start_ws_terminal 不被启动，无连接被 accept，计数自然为 0。）
    let terminal_count = crate::ws_terminal::active_terminal_count() as i32;

    debug!(
        "[GET_CONTAINER_STATUS] active breakdown: agents={}, terminals={}",
        agent_count, terminal_count
    );

    agent_count + terminal_count
}

fn get_uptime_seconds() -> i64 {
    static START_TIME: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

    let start = START_TIME.get_or_init(std::time::Instant::now);

    start.elapsed().as_secs() as i64
}
