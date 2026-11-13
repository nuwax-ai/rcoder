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

/// 解析后的查询目标：业务场景 + 该场景下用于定位容器的标识符。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResourceTarget {
    service_type: shared_types::ServiceType,
    identifier: String,
}

/// 目标解析的拒绝原因。与 HTTP 层解耦，便于单测断言。
#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetReject {
    /// 既没给 user_id 也没给 pod_id
    MissingIdentifier,
    /// user-app-builder 的容器标识是 app_id，只能经 pod_id 传
    BuilderNeedsIdentifier,
    /// user-app 业务运行容器里没有 agent_runner，本接口没有数据源
    Unsupported(shared_types::ServiceType),
}

impl TargetReject {
    fn reason_code(&self) -> &'static str {
        match self {
            // 参数问题与"场景不支持"分开：前者调用方改参数即可，后者得换接口/数据源
            Self::MissingIdentifier | Self::BuilderNeedsIdentifier => {
                shared_types::error_codes::ERR_VALIDATION
            }
            Self::Unsupported(_) => shared_types::error_codes::ERR_OPERATION_NOT_SUPPORTED,
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::MissingIdentifier => "user_id or pod_id is required".to_string(),
            Self::BuilderNeedsIdentifier => {
                "service_type=user-app-builder 的容器标识是 app_id，请用 pod_id 传入".to_string()
            }
            Self::Unsupported(service_type) => format!(
                concat!(
                    "service_type={} 不提供资源监控：该容器内没有 agent_runner，",
                    "本接口的数据源（gRPC GetContainerStatus + 容器内 /proc 扫描）不存在，",
                    "需要容器外部数据源（metrics-server / cAdvisor）另行实现"
                ),
                service_type
            ),
        }
    }
}

/// 按 `service_type` 解析目标。
///
/// 缺省 `ComputerAgentRunner`，因此不传 `service_type` 的老调用方行为不变。
/// 显式传 `user-app` 直接拒绝——它不是"查不到"，而是这条链路根本没有数据源，
/// 静默返回空数据会被读成"容器空载"。
fn resolve_resource_target(
    params: &ComputerPodResourcesQuery,
) -> Result<ResourceTarget, TargetReject> {
    let service_type = params
        .service_type
        .unwrap_or(shared_types::ServiceType::ComputerAgentRunner);
    let user_id = params
        .user_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let pod_id = params
        .pod_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // 全量匹配、不写 `_ =>`：以后新增 ServiceType 变体时这里必须编译失败，
    // 逼着明确"这个场景能不能监控"，而不是被默默当成 computer 去查。
    match service_type {
        shared_types::ServiceType::Userapp => Err(TargetReject::Unsupported(service_type)),
        shared_types::ServiceType::UserappBuilder => {
            let Some(app_id) = pod_id else {
                return Err(TargetReject::BuilderNeedsIdentifier);
            };
            Ok(ResourceTarget {
                service_type,
                identifier: app_id.to_string(),
            })
        }
        shared_types::ServiceType::ComputerAgentRunner
        | shared_types::ServiceType::ComputerNormalProject
        | shared_types::ServiceType::WebAgentRunner => {
            // computer 族与 normal-project 共容器，标识符是 user_id；
            // web-agent-runner 走同一套标识符解析（未验证是否有 agent_runner，
            // 查不到数据时按既有降级契约返回缺省字段）。
            let Some(identifier) = user_id.or(pod_id) else {
                return Err(TargetReject::MissingIdentifier);
            };
            Ok(ResourceTarget {
                service_type,
                identifier: identifier.to_string(),
            })
        }
    }
}

/// 按解析结果定位容器。
///
/// computer 族且给了 user_id 时沿用既有 `get_container_info`（带刷新语义），
/// 其余场景走 `get_container_info_by_identifier`，避免改动既有行为。
async fn locate_container(
    state: &Arc<AppState>,
    target: &ResourceTarget,
    user_id: Option<&str>,
) -> Result<Option<shared_types::ContainerBasicInfo>, AppError> {
    let computer_family = matches!(
        target.service_type,
        shared_types::ServiceType::ComputerAgentRunner
            | shared_types::ServiceType::ComputerNormalProject
    );
    if computer_family && user_id.is_some() {
        return crate::service::ComputerContainerManager::get_container_info(
            user_id.expect("checked above"),
            state.runtime(),
        )
        .await;
    }
    state
        .runtime()
        .get_container_info_by_identifier(&target.identifier, &target.service_type)
        .await
        .map_err(|e| {
            error!("[POD_RESOURCES] container lookup failed: {e}");
            AppError::internal_server_error(&format!("Failed to query container info: {e}"))
        })
}

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
    description = "按 service_type + user_id/pod_id 定位容器,返回容器 cgroup v2 级别的 CPU/内存实时用量与创建时设定的资源限额。service_type 缺省 computer-agent-runner(行为与改动前一致);computer-normal-project 与 computer 共容器(标识符同为 user_id);user-app-builder 用 app_id(pod_id)且容器内有 agent_runner,可返回进程明细与 agents;user-app(业务运行容器)没有 agent_runner 数据源,返回 ERR_OPERATION_NOT_SUPPORTED。与 /computer/agent/status(agent 进程组粒度)互补:容器口径包含 VNC/桌面环境等全部进程。资源字段在采集降级时缺省(None),不报错。"
)]
#[instrument(skip(state))]
pub async fn computer_pod_resources(
    State(state): State<Arc<AppState>>,
    I18nQuery(params): I18nQuery<ComputerPodResourcesQuery>,
) -> Result<HttpResult<ComputerPodResourcesResponse>, AppError> {
    let locale = shared_types::current_request_locale();

    // 1. 解析目标:业务场景 + 标识符。缺省 computer-agent-runner(老调用方不变)
    let target = match resolve_resource_target(&params) {
        Ok(target) => target,
        Err(reject) => {
            warn!(
                event = "pod_resources.target_rejected",
                reason = reject.reason_code(),
                detail = %reject.detail(),
                service_type = ?params.service_type,
                user_id = ?params.user_id,
                pod_id = ?params.pod_id,
                "resource target rejected"
            );
            let mut response = HttpResult::error_with_locale(reject.reason_code(), locale);
            response.error_detail = Some(
                shared_types::ErrorDetail::new(
                    reject.reason_code(),
                    "pod.resources.resolve",
                    reject.detail(),
                )
                .localized(locale),
            );
            return Ok(response);
        }
    };

    // 2. 定位容器
    let container_info = locate_container(&state, &target, params.user_id.as_deref()).await?;

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
            service_type: Some(target.service_type),
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
                service_type: Some(target.service_type),
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
    let (processes, process_count, groups) = map_process_detail(&response);
    // RPC 成功却没有明细：只有两种可能——沙箱侧没跑扫描（旧版本 agent_runner
    // 忽略 include_processes）或扫描失败。两者都会让观测端看到「没有进程 /
    // 没有 agent」，预发布环境那次空窗口正是这种形态，所以这里必须留痕。
    // 用 info 而不是 warn：混版本滚动期间旧沙箱每次轮询都会命中，warn 会刷屏。
    if process_count.is_none() {
        info!(
            event = "pod_resources.detail_absent",
            container_id = %container_info.container_id,
            user_id = ?params.user_id,
            "container status RPC succeeded but returned no process detail; observation will see empty processes/agents"
        );
    }
    let agents: Vec<shared_types::ComputerAgentExecution> =
        response.agents.iter().map(map_agent_execution).collect();

    Ok(HttpResult::success(ComputerPodResourcesResponse {
        user_id: params.user_id.clone(),
        pod_id: params.pod_id.clone(),
        service_type: Some(target.service_type),
        alive: true,
        container_cpu_percent: response.container_cpu_percent,
        container_memory_mb: response.container_memory_mb,
        container_memory_limit_mb: response.container_memory_limit_mb,
        container_cpu_limit_cores: response.container_cpu_limit_cores,
        uptime_seconds: response.uptime_seconds,
        container_id: Some(container_info.container_id),
        container_name: Some(container_info.container_name),
        processes,
        process_count,
        process_groups: groups,
        agents,
    }))
}

/// proto → 对外响应：进程明细、明细条数、分组聚合。
///
/// `process_count` 是「是否采样过」的单一事实源：agent_runner 只在
/// `include_processes=true` 时回填它（哪怕是 0），未采样时整组字段缺省。
/// 以前这里用 `!processes.is_empty()` 判断，导致「采样了但确实是 0 条」
/// 和「压根没采样」在响应里完全同形——两者都会变成 `processes: null`，
/// 观测端无法区分（预发布环境就因此把一次空扫描读成了「没有进程」）。
fn map_process_detail(
    response: &shared_types::grpc::GetContainerStatusResponse,
) -> (
    Option<Vec<shared_types::ComputerProcessEntry>>,
    Option<u32>,
    Option<shared_types::ComputerProcessGroups>,
) {
    let sampled = response.process_count.is_some();
    let processes = sampled.then(|| {
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
    let process_count = response
        .process_count
        .and_then(|count| u32::try_from(count).ok());
    let groups = (response.infra_rss_bytes.is_some()
        || response.platform_rss_bytes.is_some()
        || response.workload_rss_bytes.is_some())
    .then(|| shared_types::ComputerProcessGroups {
        infra_rss_bytes: response.infra_rss_bytes.unwrap_or(0),
        platform_rss_bytes: response.platform_rss_bytes.unwrap_or(0),
        workload_rss_bytes: response.workload_rss_bytes.unwrap_or(0),
    });
    (processes, process_count, groups)
}

/// proto → 对外响应：单个 agent 执行。
fn map_agent_execution(
    a: &shared_types::grpc::AgentExecution,
) -> shared_types::ComputerAgentExecution {
    shared_types::ComputerAgentExecution {
        project_id: a.project_id.clone(),
        session_id: (!a.session_id.is_empty()).then(|| a.session_id.clone()),
        status: a.status.clone(),
        rss_mb: a.rss_bytes as f64 / 1_048_576.0,
        cpu_seconds: a.cpu_seconds,
        process_count: a.process_count,
    }
}

/// SSE 事件里 agent 行的存活字段。
///
/// `None` = 对端没回 `agent_present`（**未知**，典型是混布旧版 agent_runner），
/// 必须保持 `null`；塌成 `false` 会把活着的老 agent 误报成已退出。显式
/// `Some(false)` 才是"确认已退出"。与 tick 失败分支的 `last_agent_present`
/// 同一口径，也符合本文件"未知保持未知"的一贯契约。
fn agent_alive_field(agent_present: Option<bool>) -> serde_json::Value {
    serde_json::json!(agent_present)
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
    description = "SSE 推送容器 cgroup 级与容器内 agent 进程组的 CPU/内存用量。service_type 语义与单次查询接口一致(缺省 computer-agent-runner,支持 computer-normal-project / user-app-builder,user-app 返回 ERR_OPERATION_NOT_SUPPORTED 且不建立流)。订阅驱动采样:连接期间按 interval_secs(默认 2s,clamp 1-30)周期采样推送;无 replay/无历史缓冲,断连即停。执行换代后自动跟随新代次;agent.alive 来自显式的执行存在性,采集降级时资源字段缺省但不会误报 agent 退出。"
)]
#[instrument(skip(state))]
pub async fn computer_pod_resources_stream(
    State(state): State<Arc<AppState>>,
    I18nQuery(params): I18nQuery<ComputerPodResourcesQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>> + use<>>, AppError> {
    // 1. 解析与定位(连接建立前完成;流建立后容器消失以缺省事件体现)
    let target = resolve_resource_target(&params).map_err(|reject| {
        warn!(
            event = "pod_resources_stream.target_rejected",
            reason = reject.reason_code(),
            detail = %reject.detail(),
            service_type = ?params.service_type,
            "resource stream target rejected"
        );
        AppError::from_code(reject.reason_code()).with_error_detail(shared_types::ErrorDetail::new(
            reject.reason_code(),
            "pod.resources.resolve",
            reject.detail(),
        ))
    })?;

    let container_info = locate_container(&state, &target, params.user_id.as_deref()).await?;

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
                            //
                            // `None` = 对端没回这个字段(**未知**), 必须原样输出
                            // null, 与下方 tick 失败分支的 `last_agent_present`
                            // 同一口径。旧实现 `unwrap_or(false)` 会把未知塌成
                            // "已退出": 混布旧版 agent_runner(proto 无该字段)时,
                            // 明明活着却被观测端判死。本文件的契约一贯是"未知保持
                            // 未知"(见 unsampled_detail_stays_absent 等用例),
                            // 客户端按 null 显示"未知", 不按 false 显示"已退出"。
                            "alive": agent_alive_field(response.agent_present),
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

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::grpc::{AgentExecution, ContainerProcess, GetContainerStatusResponse};

    /// 未采样（agent_runner 当次没跑进程扫描）→ 三组字段都缺省，
    /// 与「采样了但确实是 0 条」必须可区分。
    #[test]
    fn unsampled_detail_stays_absent() {
        let response = GetContainerStatusResponse::default();
        let (processes, count, groups) = map_process_detail(&response);
        assert!(processes.is_none(), "未采样不能报空明细");
        assert!(count.is_none());
        assert!(groups.is_none());
    }

    /// `process_count: Some(0)` 是「采样过」的显式信号 → 必须报
    /// `processes: []` 而不是缺省。
    ///
    /// 这正是之前的缺陷：用 `!processes.is_empty()` 当信号，把「采样到 0 条」
    /// 也压成 `null`，观测端只能读成「不知道」。当前 agent_runner 在扫描
    /// 失败时回 `None`（未知），因此 `Some(0)` 属于防御性契约，客户端仍必须
    /// 按「采样过」处理，不能把空明细当成缺省。
    #[test]
    fn sampled_empty_detail_is_reported_as_empty_not_absent() {
        let response = GetContainerStatusResponse {
            process_count: Some(0),
            infra_rss_bytes: Some(0),
            platform_rss_bytes: Some(0),
            workload_rss_bytes: Some(0),
            ..Default::default()
        };
        let (processes, count, groups) = map_process_detail(&response);
        assert_eq!(processes.as_deref().map(<[_]>::len), Some(0));
        assert_eq!(count, Some(0));
        let groups = groups.expect("采样的分组聚合必须出现");
        assert_eq!(
            groups.infra_rss_bytes + groups.platform_rss_bytes + groups.workload_rss_bytes,
            0
        );
    }

    /// 采样到进程 → 明细条数以 `process_count` 为准（不是 `processes.len()`，
    /// 两者不一致时以 agent_runner 的统计为准并在响应里保持自洽）。
    #[test]
    fn sampled_rows_are_mapped_with_mb_and_optional_args() {
        let response = GetContainerStatusResponse {
            processes: vec![
                ContainerProcess {
                    pid: 42,
                    comm: "python3".to_string(),
                    rss_bytes: 2 * 1_048_576,
                    cpu_seconds: 1.5,
                    group: "workload".to_string(),
                    args_head: "python3 train.py".to_string(),
                    ..Default::default()
                },
                ContainerProcess {
                    pid: 43,
                    comm: "sh".to_string(),
                    rss_bytes: 1_048_576,
                    cpu_seconds: 0.0,
                    group: "infra".to_string(),
                    args_head: String::new(),
                    ..Default::default()
                },
            ],
            process_count: Some(2),
            ..Default::default()
        };
        let (processes, count, _) = map_process_detail(&response);
        let processes = processes.expect("采样到进程必须有明细");
        assert_eq!(count, Some(2));
        assert_eq!(processes.len(), 2);
        assert_eq!(processes[0].rss_mb, 2.0);
        assert_eq!(processes[0].args_head.as_deref(), Some("python3 train.py"));
        assert!(
            processes[1].args_head.is_none(),
            "空 args_head 不应变成 Some(\"\")"
        );
    }

    /// 审查 P2：对端没回 `agent_present`（混布旧版 agent_runner）时必须报
    /// `null`（未知），不能塌成 `false`（已退出）——活着的老 agent 会被误报死亡。
    /// 显式 `Some(false)` 仍是"确认已退出"。
    #[test]
    fn agent_alive_field_keeps_unknown_distinct_from_dead() {
        assert_eq!(
            agent_alive_field(None),
            serde_json::Value::Null,
            "未知必须保持 null, 不能变成 false"
        );
        assert_eq!(agent_alive_field(Some(true)), serde_json::json!(true));
        assert_eq!(agent_alive_field(Some(false)), serde_json::json!(false));
    }

    /// ── 目标解析（service_type + 标识符）──
    fn q(
        user_id: Option<&str>,
        pod_id: Option<&str>,
        service_type: Option<shared_types::ServiceType>,
    ) -> ComputerPodResourcesQuery {
        ComputerPodResourcesQuery {
            user_id: user_id.map(str::to_string),
            pod_id: pod_id.map(str::to_string),
            service_type,
            include_processes: None,
            interval_secs: None,
        }
    }

    /// 不传 service_type：行为必须和改动前完全一致（ComputerAgentRunner + user_id）。
    #[test]
    fn resolve_defaults_to_computer_agent_runner() {
        let target = resolve_resource_target(&q(Some("42"), None, None)).expect("应解析成功");
        assert_eq!(
            target.service_type,
            shared_types::ServiceType::ComputerAgentRunner
        );
        assert_eq!(target.identifier, "42");
    }

    /// normal-project 与 computer 共容器，标识符同样是 user_id。
    #[test]
    fn resolve_normal_project_uses_user_id() {
        let target = resolve_resource_target(&q(
            Some("42"),
            None,
            Some(shared_types::ServiceType::ComputerNormalProject),
        ))
        .expect("应解析成功");
        assert_eq!(
            target.service_type,
            shared_types::ServiceType::ComputerNormalProject
        );
        assert_eq!(target.identifier, "42");
    }

    /// builder 的容器标识是 app_id，经 pod_id 传入。
    #[test]
    fn resolve_builder_uses_app_id_from_pod_id() {
        let target = resolve_resource_target(&q(
            None,
            Some("app-7"),
            Some(shared_types::ServiceType::UserappBuilder),
        ))
        .expect("应解析成功");
        assert_eq!(
            target.service_type,
            shared_types::ServiceType::UserappBuilder
        );
        assert_eq!(target.identifier, "app-7");
    }

    /// builder 只给 user_id 时必须拒绝：user_id 不是 builder 的容器标识，
    /// 用它去查会静默落到错误的容器上。
    #[test]
    fn resolve_builder_requires_pod_id() {
        let err = resolve_resource_target(&q(
            Some("42"),
            None,
            Some(shared_types::ServiceType::UserappBuilder),
        ))
        .expect_err("builder 只给 user_id 应被拒绝");
        assert_eq!(err, TargetReject::BuilderNeedsIdentifier);
        assert_eq!(err.reason_code(), shared_types::error_codes::ERR_VALIDATION);
    }

    /// user-app 业务运行容器没有 agent_runner：明确拒绝，而不是返回空数据
    /// （空数据会被读成「容器空载」）。
    #[test]
    fn resolve_rejects_userapp_with_unsupported_code() {
        let err = resolve_resource_target(&q(
            Some("42"),
            None,
            Some(shared_types::ServiceType::Userapp),
        ))
        .expect_err("user-app 应被拒绝");
        assert!(matches!(err, TargetReject::Unsupported(_)));
        assert_eq!(
            err.reason_code(),
            shared_types::error_codes::ERR_OPERATION_NOT_SUPPORTED
        );
        assert!(
            err.detail().contains("agent_runner"),
            "拒绝原因要说清数据源缺失: {}",
            err.detail()
        );
    }

    /// 两个标识符都没给 → 参数错误。
    #[test]
    fn resolve_requires_an_identifier() {
        let err = resolve_resource_target(&q(None, None, None)).expect_err("缺标识符应被拒绝");
        assert_eq!(err, TargetReject::MissingIdentifier);
        assert_eq!(err.reason_code(), shared_types::error_codes::ERR_VALIDATION);
    }

    /// agent 行：空 session_id 不能变成 `Some("")`（前端会当成真实 id）。
    #[test]
    fn agent_mapping_drops_empty_session_id() {
        let mapped = map_agent_execution(&AgentExecution {
            project_id: "p1".to_string(),
            session_id: String::new(),
            status: "Idle".to_string(),
            rss_bytes: 3 * 1_048_576,
            cpu_seconds: 2.0,
            process_count: 7,
            ..Default::default()
        });
        assert!(mapped.session_id.is_none());
        assert_eq!(mapped.rss_mb, 3.0);
        assert_eq!(mapped.process_count, 7);
    }
}
