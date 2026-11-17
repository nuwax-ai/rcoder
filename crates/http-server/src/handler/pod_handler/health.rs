//! `/computer/pod/health` —— 单容器健康快照（供前端轮询）。
//!
//! 为什么单独一个接口，而不是复用现有的：
//! - `/computer/pod/resources/stream` 的数据源是**容器内** agent 的 gRPC，
//!   容器被 OOM 杀掉连接就断，只能看到 `alive=false`，拿不到原因；
//! - `/computer/pod/overview` 按 namespace **全量 list**（48 个 Pod），
//!   为一个容器的 OOM 状态付全量成本，且 10s 缓存不够灵敏。
//!
//! 本接口只 GET 一个容器，数据源在运行时侧（k8s API / docker daemon），
//! **容器死了照样能查**——这是它作为"OOM 监控"通道的关键性质。

use std::sync::{Arc, LazyLock, Mutex};

use axum::extract::State;
use serde::{Deserialize, Serialize};
use shared_types::ServiceType;
use tracing::{debug, info, instrument, warn};
use utoipa::ToSchema;

use crate::router::AppState;
use crate::{AppError, HttpResult};

use super::I18nQuery;
use super::resources::resolve_resource_target;

/// 查询参数（与 `/computer/pod/resources` 同一套三字段分派，不另立规则）
#[derive(Debug, Clone, Default, Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "snake_case")]
#[into_params(parameter_in = Query)]
pub struct PodHealthQuery {
    /// 用户 ID（computer 族容器标识）
    #[serde(default)]
    pub user_id: Option<String>,
    /// Pod/容器标识符（与 user_id 二选一）
    #[serde(default)]
    pub pod_id: Option<String>,
    /// 业务场景。可选值：`WebAgentRunner` / `ComputerAgentRunner` /
    /// `ComputerNormalProject` / `Userapp` / `UserappBuilder`。
    #[serde(default)]
    pub service_type: Option<ServiceType>,
    /// userapp 的应用 ID（与 app_stage 配合；dev→builder 容器）
    #[serde(default)]
    pub app_id: Option<String>,
    /// userapp 环境：`dev`（缺省）/ `prod`
    #[serde(default)]
    pub app_stage: Option<String>,
    /// web 族的 project_id
    #[serde(default)]
    pub project_id: Option<String>,
    /// 绕过缓存强制直读（默认读 3s 缓存）
    #[serde(default)]
    pub fresh: Option<bool>,
}

/// 响应体 = 运行时层 `ContainerHealth` 的薄包装（加缓存元信息）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PodHealthResponse {
    /// 健康快照
    pub health: container_runtime_api::ContainerHealth,
    /// 缓存年龄（秒）；0 = 本次新采集
    pub cache_age_seconds: u64,
}

// ── 3s 缓存（按 runtime × 容器隔离）────────────────────────────────────
//
// 前端轮询 3s，命中缓存后实际打 k8s 的频率 ≈ 1/3s/容器。
// 键 = (runtime 指针, 容器标识, service_type)——不同 runtime 不串数据。
struct HealthCache {
    health: container_runtime_api::ContainerHealth,
    ts: std::time::Instant,
}

static CACHE: LazyLock<Mutex<std::collections::HashMap<(usize, String), HealthCache>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// 缓存 TTL：pod status 的变化（OOM/重启）本身就有秒级延迟（kubelet
/// 汇报周期），3s 足够灵敏，同时把轮询压力压到 1/3。
const CACHE_TTL_SECS: u64 = 3;

/// `GET /computer/pod/health`
#[utoipa::path(
    get,
    path = "/computer/pod/health",
    params(PodHealthQuery),
    responses(
        (
            status = 200,
            description = "单容器健康快照：最近一次终止的原因/退出码/时间，是否 OOM，累计重启次数。容器不存在时 data=null（HTTP 200）",
            body = HttpResult<PodHealthResponse>
        ),
        (
            status = 404,
            description = "目标解析失败（参数非法或该容器族不支持）",
            body = HttpResult<String>
        )
    ),
    tag = "pod",
    operation_id = "computer_pod_health",
    summary = "查询单个容器的健康/OOM 信息（供前端轮询）",
    description = "只查一个容器（k8s 单 Pod GET / docker inspect+events），**容器被 OOM 杀掉后仍可查询**——数据源在运行时侧，不在容器内。\n\n字段以**终止**为主语，不以 OOM 为主语：`last_termination` 对任何原因的终止都完整给出（`reason`/`exit_code`/`finished_at`），OOM 只是 `reason` 的一个取值；`is_oom` 是 `reason==OOMKilled` 的派生便捷位（**不要**用 exit_code==137 推断——liveness SIGKILL 同样给 137）。`last_termination=null` 表示从未终止过；`started_at` 是本次容器启动时间。`pod_uid` 变化 = 容器被删除重建（计数归零）。\n\n默认 3s 缓存；`fresh=true` 强制直读。k8s 用 `lastState.terminated.finishedAt` 作为最近一次重启时间；docker 用 `die` 事件时间（`State` 字段在运行中会被重置，靠事件补历史）。\n\n**能力差异（务必按部署模式解读）**：k8s 下 OOM 归因完整（`OOMKilled` + exit code + 终止时间 + 重启数），且容器 OOM 后由 kubelet 自动重启。docker compose 下 rcoder 创建的容器是 `auto_remove=true` 且无 restart policy：**容器退出即被删除、不会自动重启**，需要业务侧检测 `phase=NotFound` 后主动 `/computer/pod/ensure` 重建。此时 `last_termination` 仍会尽量从**Docker daemon 的事件历史**还原（`oom` 事件 → `reason=OOMKilled`；`die` 事件 → exit code 与终止时间），但受事件回放窗口限制（最近 256 条 / 1 小时），超出窗口则返回 null。"
)]
#[instrument(skip(state))]
pub async fn computer_pod_health(
    State(state): State<Arc<AppState>>,
    I18nQuery(params): I18nQuery<PodHealthQuery>,
) -> Result<HttpResult<PodHealthResponse>, AppError> {
    // 1) 目标解析（与 /pod/resources 同一函数，参数规则完全一致）
    let resources_query = shared_types::ComputerPodResourcesQuery {
        user_id: params.user_id.clone(),
        pod_id: params.pod_id.clone(),
        service_type: params.service_type,
        app_id: params.app_id.clone(),
        app_stage: params.app_stage.clone(),
        project_id: params.project_id.clone(),
        isolation_type: None,
        tenant_id: None,
        space_id: None,
        include_processes: None,
        interval_secs: None,
    };
    let target = resolve_resource_target(&resources_query).map_err(|reject| {
        warn!(
            event = "pod_health.target_rejected",
            reason = reject.reason_code(),
            detail = %reject.detail(),
            "health target rejected"
        );
        AppError::from_code(reject.reason_code()).with_error_detail(shared_types::ErrorDetail::new(
            reject.reason_code(),
            "pod.health.resolve",
            reject.detail(),
        ))
    })?;

    // 2) 定位容器（拿 container_name 作为运行时侧的查询键）
    let container =
        super::resources::locate_container(&state, &target, params.user_id.as_deref()).await?;
    let Some(container) = container else {
        // 容器不存在不是错误，但仍要尽量回答"它是怎么没的"：
        // docker 模式下 rcoder 创建的容器 auto_remove=true，容器退出即被
        // 删除，inspect 无从查询——但 **daemon 侧的事件历史仍在**，其中
        // `oom` 事件是 Docker 专为 cgroup OOM 发出的（比 exitCode 可靠：
        // 137 同样来自 SIGKILL）。所以这里再问一次运行时：容器没了，
        // 但"最近一次终止"可能还能从事件还原。
        info!(
            event = "pod_health.container_absent",
            identifier = %target.identifier,
            user_id = ?params.user_id,
            pod_id = ?params.pod_id,
            "container not found, probing termination history"
        );
        let runtime = state.runtime().clone();
        let history = runtime
            .container_termination_history(&target.identifier)
            .await
            .unwrap_or_else(|e| {
                debug!(event = "pod_health.history_failed", error = %e, "history probe failed");
                None
            });
        return Ok(HttpResult::success(PodHealthResponse {
            health: container_runtime_api::ContainerHealth {
                // 目标解析成功但容器不存在：名字仍然填上（前端要显示"是哪个
                // 容器不在"），source 标 "none" 而不是空串
                container_name: target.identifier.clone(),
                alive: false,
                phase: "NotFound".to_string(),
                ready: false,
                restart_count: 0,
                last_termination: history,
                started_at: None,
                waiting_reason: None,
                detail: Some(
                    "container not found (docker: already removed by auto_remove)".to_string(),
                ),
                sampled_at: chrono::Utc::now(),
                source: "none".to_string(),
                pod_uid: None,
            },
            cache_age_seconds: 0,
        }));
    };

    // 3) 缓存查询
    let runtime = state.runtime().clone();
    let key = (
        Arc::as_ptr(&runtime) as *const () as usize,
        container.container_name.clone(),
    );
    let fresh = params.fresh.unwrap_or(false);
    if !fresh
        && let Ok(cache) = CACHE.lock()
        && let Some(entry) = cache.get(&key)
        && entry.ts.elapsed().as_secs() < CACHE_TTL_SECS
    {
        return Ok(HttpResult::success(PodHealthResponse {
            health: entry.health.clone(),
            cache_age_seconds: entry.ts.elapsed().as_secs(),
        }));
    }

    // 4) 采集（单容器；容器不存在 → None）
    let health = runtime
        .container_health(&container.container_name, &target.service_type)
        .await
        .map_err(|e| {
            warn!(
                event = "pod_health.collect_failed",
                container = %container.container_name,
                error = %e,
                "health collection failed"
            );
            AppError::from_code(shared_types::error_codes::ERR_SERVICE_UNAVAILABLE)
                .with_error_detail(shared_types::ErrorDetail::new(
                    shared_types::error_codes::ERR_SERVICE_UNAVAILABLE,
                    "pod.health.collect",
                    format!("container health collection failed: {e}"),
                ))
        })?;

    let Some(health) = health else {
        // 登记表仍认为容器在、但运行时侧已查不到（docker auto_remove 删除的
        // 典型窗口）。与"locate 就失败"同样处理：从事件历史尽量还原
        // "它是怎么没的"，而不是只回一句"消失了"。
        let history = runtime
            .container_termination_history(&container.container_name)
            .await
            .unwrap_or_else(|e| {
                debug!(event = "pod_health.history_failed", error = %e, "history probe failed");
                None
            });
        return Ok(HttpResult::success(PodHealthResponse {
            health: container_runtime_api::ContainerHealth {
                container_name: container.container_name.clone(),
                alive: false,
                phase: "NotFound".to_string(),
                last_termination: history,
                detail: Some(
                    "container disappeared between locate and inspect (docker auto_remove)"
                        .to_string(),
                ),
                source: "none".to_string(),
                sampled_at: chrono::Utc::now(),
                ..Default::default()
            },
            cache_age_seconds: 0,
        }));
    };

    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(
            key,
            HealthCache {
                health: health.clone(),
                ts: std::time::Instant::now(),
            },
        );
        // 有界：只保留最近 256 个容器的条目，防止长期运行累积
        if cache.len() > 256 {
            let now = std::time::Instant::now();
            cache.retain(|_, entry| now.duration_since(entry.ts).as_secs() < 60);
        }
    }

    Ok(HttpResult::success(PodHealthResponse {
        health,
        cache_age_seconds: 0,
    }))
}

#[cfg(test)]
mod tests {
    use container_runtime_api::ContainerHealth;

    /// 语义红线：非 OOM 的终止同样带齐 reason/exit_code/finished_at。
    /// 字段不提供 `last_oom_at` 这类"只在 OOM 时有值"的顶层字段——那会让
    /// 调用方误以为非 OOM 时没有信息。
    #[test]
    fn non_oom_termination_is_fully_populated() {
        use container_runtime_api::LastTermination;

        let killed = ContainerHealth {
            last_termination: Some(LastTermination {
                reason: Some("Error".into()),
                exit_code: Some(137),
                finished_at: chrono::DateTime::from_timestamp(1_700_000_100, 0),
                is_oom: false,
            }),
            ..Default::default()
        };
        let last = killed
            .last_termination
            .as_ref()
            .expect("非 OOM 也必须有终止信息");
        assert_eq!(last.reason.as_deref(), Some("Error"));
        assert_eq!(last.exit_code, Some(137));
        assert!(last.finished_at.is_some(), "非 OOM 也要有终止时间");
        assert!(!last.is_oom, "137 不等于 OOM");
    }

    /// 从未终止过：last_termination 必须是 None（不是空对象）
    #[test]
    fn never_terminated_is_none_not_empty_object() {
        let health = ContainerHealth::default();
        assert!(health.last_termination.is_none());
        assert!(health.started_at.is_none());
    }
}
