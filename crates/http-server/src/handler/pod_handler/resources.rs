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
            processes: None,
            process_count: None,
            process_groups: None,
            agents: Vec::new(),
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

    let response = match crate::grpc::status_query::query_container_status_with_options(
        &state.grpc_pool,
        &grpc_addr,
        params.user_id.as_deref().unwrap_or_default(),
        &container_info.project_id,
        None,
        std::time::Duration::from_secs(shared_types::GRPC_CONTAINER_STATUS_TIMEOUT_SECS),
        params.include_processes.unwrap_or(true),
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
                processes: None,
                process_count: None,
                process_groups: None,
                agents: Vec::new(),
            }));
        }
    };

    info!(
        "✅ [POD_RESOURCES] container resources: container_id={}, cpu={:?}, mem={:?}MB",
        container_info.container_id, response.container_cpu_percent, response.container_memory_mb
    );

    // 进程明细+分组聚合(proto → 响应;MB 换算,分组字符串透传)
    let processes = (!response.processes.is_empty()).then(|| {
        response
            .processes
            .iter()
            .map(|p| shared_types::ComputerProcessEntry {
                pid: p.pid,
                comm: p.comm.clone(),
                rss_mb: p.rss_bytes as f64 / 1_048_576.0,
                cpu_seconds: p.cpu_seconds,
                group: p.group.clone(),
                args_head: (!p.args_head.is_empty()).then(|| p.args_head.clone()),
            })
            .collect()
    });
    let agents: Vec<shared_types::ComputerAgentExecution> = response
        .agents
        .iter()
        .map(|a| shared_types::ComputerAgentExecution {
            project_id: a.project_id.clone(),
            session_id: (!a.session_id.is_empty()).then(|| a.session_id.clone()),
            status: a.status.clone(),
            rss_mb: a.rss_bytes as f64 / 1_048_576.0,
            cpu_seconds: a.cpu_seconds,
            process_count: a.process_count,
        })
        .collect();
    let groups = (response.infra_rss_bytes.is_some()
        || response.platform_rss_bytes.is_some()
        || response.workload_rss_bytes.is_some())
    .then(|| shared_types::ComputerProcessGroups {
        infra_rss_bytes: response.infra_rss_bytes.unwrap_or(0),
        platform_rss_bytes: response.platform_rss_bytes.unwrap_or(0),
        workload_rss_bytes: response.workload_rss_bytes.unwrap_or(0),
    });

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
        processes,
        process_count: (!response.processes.is_empty()).then_some(response.processes.len() as u32),
        process_groups: groups,
        agents,
    }))
}

// ==== SSE 流式版本 ====
//
// 与单次查询接口的分工:本流面向前端连续监控(图表/实时面板),订阅驱动
// 采样(有连接才采样,断连即停);单次查询接口用于低频人工查看。资源数据
// 无 replay 价值,流内不做历史缓冲(直发,断连即弃)。

use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::Stream;
use std::convert::Infallible;

/// 查询 Computer 容器资源监控流
///
/// SSE 推送容器级(cgroup v2)与 agent 进程级(进程组)资源用量。
#[utoipa::path(
    get,
    path = "/computer/pod/resources/stream",
    params(ComputerPodResourcesQuery),
    responses(
        (
            status = 200,
            description = "SSE 资源监控流。event=resources 的 data 为 JSON:\n\
              `{ts, container:{alive,cpu_percent,memory_mb,memory_limit_mb,cpu_limit_cores}, agent:{alive,status,cpu_percent,memory_mb}}`。\n\
              agent 字段为容器内当前执行的实时观测(执行换代后自动跟随新代次);\n\
              采集降级时对应字段缺省;连续失败将关断流。Content-Type: text/event-stream",
        ),
        (
            status = 400,
            description = "请求参数错误(user_id/pod_id 均缺省)",
            body = HttpResult<String>
        ),
        (
            status = 404,
            description = "找不到指定的容器(连接建立前定位失败)",
            body = HttpResult<String>
        )
    ),
    tag = "pod",
    operation_id = "computer_pod_resources_stream",
    summary = "订阅 Computer 容器资源监控流(SSE,容器 + agent)",
    description = "SSE 推送容器 cgroup 级与容器内 agent 进程组的 CPU/内存用量。订阅驱动采样:连接期间按 interval_secs(默认 2s,clamp 1-30)周期采样推送;无 replay/无历史缓冲,断连即停。执行换代后自动跟随新代次;agent.alive 来自显式的执行存在性,采集降级时资源字段缺省但不会误报 agent 退出。"
)]
#[instrument(skip(state))]
pub async fn computer_pod_resources_stream(
    State(state): State<Arc<AppState>>,
    I18nQuery(params): I18nQuery<ComputerPodResourcesQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>> + use<>>, AppError> {
    // 1. 验证与定位(连接建立前完成;流建立后容器消失以缺省事件体现)
    let has_user_id = params
        .user_id
        .as_ref()
        .is_some_and(|s| !s.trim().is_empty());
    let has_pod_id = params.pod_id.as_ref().is_some_and(|s| !s.trim().is_empty());
    if !has_user_id && !has_pod_id {
        return Err(AppError::validation_error("user_id or pod_id is required"));
    }

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
                error!("[POD_RESOURCES_STREAM] container lookup failed: {e}");
                AppError::internal_server_error(&format!("Failed to query container info: {e}"))
            })?
    };

    let Some(container_info) = container_info else {
        warn!(
            "[POD_RESOURCES_STREAM] Container not found: user_id={:?}, pod_id={:?}",
            params.user_id, params.pod_id
        );
        return Err(AppError::from_code(
            shared_types::error_codes::ERR_CONTAINER_NOT_FOUND,
        ));
    };

    let grpc_addr = shared_types::build_grpc_addr(
        &container_info.container_name,
        &container_info.container_ip,
        &state.config.app_manager.namespace,
        &state.cluster_domain,
    )
    .map_err(|e| {
        error!("[POD_RESOURCES_STREAM] failed to build grpc addr: {e}");
        AppError::internal_server_error(&format!("Failed to build grpc address: {e}"))
    })?;

    info!(
        "📡 [POD_RESOURCES_STREAM] stream opened: container_id={}, user_id={:?}, pod_id={:?}",
        container_info.container_id, params.user_id, params.pod_id
    );

    // 2. 采样循环:订阅驱动,断连即停(SSE drop 取消 stream future)
    let tick_interval = std::time::Duration::from_secs(
        shared_types::computer_pod_resources_clamp_interval(params.interval_secs),
    );
    let max_consecutive_failures: u32 = 15; // ~30s 全失败则关流,避免僵尸流
    let grpc_pool = state.grpc_pool.clone();
    let user_id = params.user_id.clone();
    let project_id = container_info.project_id.clone();
    let container_id = container_info.container_id.clone();
    let alive_at_open = super::is_container_running(&container_info.status);

    let stream = async_stream::stream! {
        let mut consecutive_failures: u32 = 0;
        // 最近一次成功采样看到的 agent 存在性:单次 tick 失败不能把活着的
        // agent 报成已退出(采集降级 ≠ agent 退出),未知时为 None。
        let mut last_agent_present: Option<bool> = None;
        loop {
            let tick = crate::grpc::status_query::query_container_status_with_options(
                &grpc_pool,
                &grpc_addr,
                user_id.as_deref().unwrap_or_default(),
                &project_id,
                None,
                std::time::Duration::from_secs(shared_types::GRPC_CONTAINER_STATUS_TIMEOUT_SECS),
                params.include_processes.unwrap_or(true),
            )
            .await;

            let event = match tick {
                Ok(response) => {
                    consecutive_failures = 0;
                    last_agent_present = response.agent_present;
                    serde_json::json!({
                        "ts": chrono::Utc::now().to_rfc3339(),
                        "container": {
                            "alive": true,
                            "cpu_percent": response.container_cpu_percent,
                            "memory_mb": response.container_memory_mb,
                            "memory_limit_mb": response.container_memory_limit_mb,
                            "cpu_limit_cores": response.container_cpu_limit_cores,
                        },
                        "agent": {
                            // 存活用显式的 agent_present,不用资源字段推断:
                            // 采集降级时 cpu_percent/memory_mb 同为 None,
                            // 会把活着的 agent 显示成已退出。
                            "alive": response.agent_present.unwrap_or(false),
                            "status": response.status,
                            "cpu_percent": response.cpu_percent,
                            "memory_mb": response.memory_mb,
                        },
                        // 容器内进程明细+分组聚合(仅 include_processes 请求携带)
                        "processes": if response.processes.is_empty() {
                            None
                        } else {
                            Some(
                                response
                                    .processes
                                    .iter()
                                    .map(|p| {
                                        serde_json::json!({
                                            "pid": p.pid,
                                            "comm": p.comm,
                                            "rss_mb": p.rss_bytes as f64 / 1_048_576.0,
                                            "cpu_seconds": p.cpu_seconds,
                                            "group": p.group,
                                        })
                                    })
                                    .collect::<Vec<_>>(),
                            )
                        },
                        // 每个 project 一条 agent 进程组聚合(仅 include_processes 时携带)
                        "agents": response.agents.iter().map(|a| serde_json::json!({
                            "project_id": a.project_id,
                            "session_id": a.session_id,
                            "status": a.status,
                            "rss_mb": a.rss_bytes as f64 / 1_048_576.0,
                            "cpu_seconds": a.cpu_seconds,
                            "process_count": a.process_count,
                        })).collect::<Vec<_>>(),
                        "process_groups": if response.infra_rss_bytes.is_none()
                            && response.platform_rss_bytes.is_none()
                            && response.workload_rss_bytes.is_none()
                        {
                            None
                        } else {
                            Some(serde_json::json!({
                                "infra_rss_mb": response.infra_rss_bytes.unwrap_or(0) as f64 / 1_048_576.0,
                                "platform_rss_mb": response.platform_rss_bytes.unwrap_or(0) as f64 / 1_048_576.0,
                                "workload_rss_mb": response.workload_rss_bytes.unwrap_or(0) as f64 / 1_048_576.0,
                            }))
                        },
                    })
                }
                Err(e) => {
                    consecutive_failures += 1;
                    warn!(
                        "[POD_RESOURCES_STREAM] tick failed ({consecutive_failures}/{max_consecutive_failures}): container_id={}, error={e}",
                        container_id
                    );
                    serde_json::json!({
                        "ts": chrono::Utc::now().to_rfc3339(),
                        "container": {"alive": alive_at_open && consecutive_failures <= 1},
                        // 沿用上次已知值:一次查询失败不代表 agent 退出
                        "agent": {"alive": last_agent_present},
                    })
                }
            };

            yield Ok(
                Event::default()
                    .event("resources")
                    .data(serde_json::to_string(&event).unwrap_or_default()),
            );

            if consecutive_failures >= max_consecutive_failures {
                error!(
                    "[POD_RESOURCES_STREAM] closing stream after consecutive failures: container_id={}",
                    container_id
                );
                break;
            }

            tokio::time::sleep(tick_interval).await;
        }
        info!(
            "📡 [POD_RESOURCES_STREAM] stream closed: container_id={}",
            container_id
        );
    };

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("keep-alive"),
    ))
}
