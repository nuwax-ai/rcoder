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
use super::vnc_probe::probe_vnc_readiness;

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
                .filter(|owner| req.project_id.is_empty() || *owner == req.project_id)
        } else if !req.project_id.is_empty() {
            Some(req.project_id.clone())
        } else {
            info!("[gRPC] GetStatus: all parameters are empty, returning not_found");
            return Ok(Response::new(GetStatusResponse {
                status: "not_found".to_string(),
                is_found: false,
                stream_epoch: None,
            }));
        };

        let (status_str, is_found) = if let Some(ref pid) = project_id {
            if let Some(agent_info) = AGENT_REGISTRY.get_agent_info(pid)
                .filter(|info| req.session_id.is_empty() || info.session_id.to_string() == req.session_id) {
                let status_str = match agent_info.status {
                    AgentStatus::Pending => "busy",
                    AgentStatus::Active => "busy",
                    AgentStatus::Idle => "idle",
                    AgentStatus::Terminating => "busy",
                };
                (status_str, true)
            } else if req.session_id.is_empty() && AGENT_REGISTRY.is_pending(pid) {
                // Project preparation is busy even before ACP assigns a real
                // session. Never publish a session identity or stream epoch for it.
                ("busy", true)
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

        // session 的 stream epoch(供 rcoder 判断 seq epoch 是否变化,#15)。
        let stream_epoch = if is_found && !req.session_id.is_empty() {
            crate::service::SESSION_CACHE.view(&req.session_id, |_, d| d.epoch().to_string())
        } else {
            None
        };

        Ok(Response::new(GetStatusResponse {
            status: status_str.to_string(),
            is_found,
            stream_epoch,
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

        let connections = user_connection_count().map_err(|error| {
            Status::unavailable(format!("Container connection activity unavailable: {error}"))
        })?;
        let active_tasks = get_active_tasks_count(connections).saturating_add(
            i32::try_from(connections).unwrap_or(i32::MAX),
        );
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
            idle_observation_version: u32::from(cfg!(target_os = "linux")),
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

fn get_active_tasks_count(connections: usize) -> i32 {
    let agent_count = AGENT_REGISTRY
        .iter_agents()
        .filter(|entry| match entry.value().status {
            AgentStatus::Idle => false,
            AgentStatus::Pending | AgentStatus::Active | AgentStatus::Terminating => true,
        })
        .count() as i32;

    // 终端 WS 连接也算「容器在用」：终端流量本身不计入 agent active task，
    // 这里把它叠加进 active_tasks，使 idle cleaner 的 gRPC 二次确认能拦下「终端在用」的容器。
    // （http-server 关闭时 start_ws_terminal 不被启动，无连接被 accept，计数自然为 0。）
    let terminal_count = crate::ws_terminal::active_terminal_count() as i32;

    // VNC 桌面连接同样算「容器在用」：VNC 数据流不经过 agent_runner（Pingora 直连容器 6080），
    // 故读 /proc/net/tcp 数 noVNC 端口上的 ESTABLISHED 连接。用户开着虚拟桌面阅读、
    // 但两次对话间 agent 处于 Idle 时，靠这个计数让容器被判活跃、不被闲置回收。
    let vnc_count = crate::vnc_activity::active_vnc_client_count() as i32;

    // Embedded UserApp build/start/restart workers continue after HTTP 202.
    // Task polling is read-only and does not refresh project activity. Count
    // their real execution so an idle scan cannot reap a building container.
    // Explicit Stop/Restart still follows its normal cancellation path.
    let local_workers =
        i32::try_from(process_utils::workers::active_in_process()).unwrap_or(i32::MAX);
    let http_requests =
        i32::try_from(process_utils::request_activity::active_in_process()).unwrap_or(i32::MAX);

    // `connections` belongs in the same breakdown: an over-count of resident
    // loopback IPC (legacy images without the peer filter) is only diagnosable
    // when the component that produced it is visible next to the others.
    debug!(
        "[GET_CONTAINER_STATUS] active breakdown: agents={}, terminals={}, vnc={}, local_workers={}, http_requests={}, connections={}",
        agent_count, terminal_count, vnc_count, local_workers, http_requests, connections
    );

    agent_count
        .saturating_add(i32::try_from(AGENT_REGISTRY.pending_preparations()).unwrap_or(i32::MAX))
        .saturating_add(terminal_count)
        .saturating_add(vnc_count)
        .saturating_add(local_workers)
        .saturating_add(http_requests)
}

/// Preview/HTTP/WebSocket traffic bypasses the agent registry. Count inbound
/// established sockets on listening user ports, excluding the probing RPC itself.
#[cfg(target_os = "linux")]
fn user_connection_count() -> anyhow::Result<usize> {
    use procfs::net::{TcpState, tcp, tcp6};
    let entries = tcp()?.into_iter().chain(tcp6()?).collect::<Vec<_>>();
    let listening = entries
        .iter()
        .filter(|entry| entry.state == TcpState::Listen)
        .map(|entry| entry.local_address.port())
        .collect::<std::collections::HashSet<_>>();
    Ok(entries
        .iter()
        .filter(|entry| {
            user_connection(
                entry.local_address.port(),
                entry.state == TcpState::Established,
                listening.contains(&entry.local_address.port()),
                peer_is_loopback(&entry.remote_address),
            )
        })
        .count())
}

#[cfg(not(target_os = "linux"))]
fn user_connection_count() -> anyhow::Result<usize> {
    Ok(0)
}

#[cfg(any(target_os = "linux", test))]
fn user_connection(port: u16, established: bool, listening: bool, remote_loopback: bool) -> bool {
    // Loopback peers are intra-container IPC (a resident tool's local
    // socketpairs — opencode keeps several alive forever), not user traffic.
    // Future agents running this code would otherwise hold new containers
    // at active_tasks >= 3 whenever such a tool runs (the pre-70f30aab
    // agents in legacy containers lacked this counter entirely).
    established && listening && port != shared_types::GRPC_DEFAULT_PORT && !remote_loopback
}

/// Loopback check covering both native IPv6 (`::1`) and IPv4-mapped IPv6
/// (`::ffff:127.0.0.1`) — `Ipv6Addr::is_loopback` alone misses the mapped
/// form, which `/proc/net/tcp6` reports for IPv4 loopback sockets on
/// dual-stack listeners.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn peer_is_loopback(addr: &std::net::SocketAddr) -> bool {
    match addr.ip() {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

fn get_uptime_seconds() -> i64 {
    static START_TIME: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

    let start = START_TIME.get_or_init(std::time::Instant::now);

    start.elapsed().as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preparing_without_a_session_keeps_container_active() {
        let project = "container-status-preparation";
        assert_eq!(get_active_tasks_count(0), 0);
        AGENT_REGISTRY.set_pending(project);
        assert!(get_active_tasks_count(0) > 0);
        AGENT_REGISTRY.clear_pending_if_exists(project);
        assert_eq!(get_active_tasks_count(0), 0);
    }

    #[test]
    fn preview_connections_protect_compute_but_grpc_probes_closed_sockets_and_loopback_ipc_do_not()
    {
        assert!(user_connection(80, true, true, false));
        assert!(user_connection(4000, true, true, false));
        assert!(!user_connection(
            shared_types::GRPC_DEFAULT_PORT,
            true,
            true,
            false
        ));
        assert!(!user_connection(4000, false, true, false));
        assert!(!user_connection(49152, true, false, false));
        // A resident tool's intra-container loopback connection lands on a
        // listening local port and is established — without the loopback
        // exclusion it counts as user traffic forever.
        assert!(!user_connection(34843, true, true, true));
        assert!(!user_connection(80, true, true, true));
    }

    #[test]
    fn loopback_detection_covers_mapped_ipv6_and_rejects_remote_addrs() {
        use std::net::SocketAddr;
        let v4: SocketAddr = "127.0.0.1:44580".parse().unwrap();
        let v6: SocketAddr = "[::1]:44580".parse().unwrap();
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:44580".parse().unwrap();
        let remote: SocketAddr = "172.18.0.2:43980".parse().unwrap();
        let remote_v6: SocketAddr = "[::ffff:172.18.0.2]:43980".parse().unwrap();
        assert!(peer_is_loopback(&v4));
        assert!(peer_is_loopback(&v6));
        assert!(
            peer_is_loopback(&mapped),
            "Ipv6Addr::is_loopback alone misses the v4-mapped form /proc/net/tcp6 reports"
        );
        assert!(!peer_is_loopback(&remote));
        assert!(!peer_is_loopback(&remote_v6));
    }

    #[tokio::test]
    async fn embedded_worker_keeps_container_active_until_cancelled_execution_exits() {
        let registry = process_utils::workers::WorkerRegistry::new(None);
        let (release, released) = tokio::sync::oneshot::channel();
        let worker = registry
            .spawn(async move { released.await.unwrap() })
            .unwrap();
        assert!(get_active_tasks_count(0) > 0);
        registry.close().unwrap();
        assert!(
            get_active_tasks_count(0) > 0,
            "cancellation is not execution exit"
        );
        release.send(()).unwrap();
        worker.await.unwrap().unwrap();
        assert_eq!(process_utils::workers::active_in_process(), 0);
    }
}
