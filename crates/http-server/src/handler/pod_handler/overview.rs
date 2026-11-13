//! `GET /computer/pod/overview`：命名空间级 Pod 概览（方案：docs/pod-overview.md）。
//!
//! 与 `/computer/pod/resources`（单容器实时明细）互补：本接口一次返回
//! namespace（k8s）/ daemon（docker）内**全部**容器的规格、健康与最近事件。
//!
//! P1 范围：spec + health + events；usage（实际用量）是 P2。
//!
//! 关键契约：
//! - **10s 缓存 + 单飞**：上游是 4 次 list（k8s）/ N 次 inspect+stats（docker），
//!   面板轮询不允许每次都打穿到 API server；
//! - **过滤在缓存快照上做**：`search` 等参数改变**不**触发新采集；
//! - Pod 列表失败 → 503（`error_detail.stage=pod.overview.list`）；
//!   分区失败（events/pvc）→ `degraded[]` 标注，不影响状态码。

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use axum::extract::{Query, State};
use tracing::{error, info, instrument, warn};

use crate::router::AppState;
use crate::{AppError, HttpResult};
use container_runtime_api::pod_overview::{PodOverviewFilter, PodOverviewSnapshot, apply_filter};
use utoipa::ToSchema;

use serde::Deserialize;

/// 请求参数。
#[derive(Debug, Clone, Default, Deserialize, utoipa::IntoParams, ToSchema)]
pub struct PodOverviewQuery {
    /// 关键字过滤（大小写不敏感子串：pod 名 / user_id / app_id / project_id / pod_id）
    #[serde(default)]
    pub search: Option<String>,
    /// 每 Pod 返回最近 N 条事件（默认 20，上限 50）
    #[serde(default)]
    pub events_limit: Option<u32>,
    /// 按业务场景过滤（computer-agent-runner / computer-normal-project /
    /// web-agent-runner / userapp / userapp-builder）
    #[serde(default)]
    pub service_type: Option<String>,
    /// 按 computer 族的 user_id 过滤
    #[serde(default)]
    pub user_id: Option<String>,
    /// 按 userapp 族的 app_id 过滤
    #[serde(default)]
    pub app_id: Option<String>,
    /// 按 web 族的 project_id 过滤
    #[serde(default)]
    pub project_id: Option<String>,
    /// 是否在 spec.containers/usage.containers 里带上内部 sidecar
    /// （log-shipper / log-collector；默认隐藏）
    #[serde(default)]
    pub include_sidecars: Option<bool>,
    /// 返回条数上限（默认 500）
    #[serde(default)]
    pub limit: Option<u32>,
}

/// 对外响应（HttpResult data）。快照类型直接复用运行时层的
/// `PodOverviewSnapshot`——它已经是 Serialize 的，且字段语义（null≠0、
/// degraded 分区标注）在类型层就定死了，再抄一份只会漂移。
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
pub struct PodOverviewResponse {
    /// 数据源形态："kubernetes" | "docker"
    pub source: String,
    /// namespace（docker 下为 None）
    pub namespace: Option<String>,
    pub generated_at: chrono::DateTime<chrono::Utc>,
    /// 快照年龄（秒）；>0 表示命中缓存
    pub cache_age_seconds: u64,
    /// 汇总计数（全量，不受过滤影响——面板显示"84 pods"而不是过滤后的 3）
    pub summary: PodOverviewSummary,
    /// 分区降级说明；空 = 全部成功
    pub degraded: Vec<container_runtime_api::pod_overview::PodOverviewDegraded>,
    /// 过滤后的 Pod 列表
    pub pods: Vec<container_runtime_api::pod_overview::PodOverviewItem>,
}

/// 汇总（在**全量**快照上算，不受过滤影响）。
#[derive(Debug, Clone, Default, serde::Serialize, ToSchema)]
pub struct PodOverviewSummary {
    pub pods: usize,
    pub running: usize,
    pub not_ready: usize,
    pub restarted: usize,
    pub oom_killed_last: usize,
    pub with_warning_events: usize,
}

// ── 缓存（复审 R10 修正）───────────────────────────────────────────────
//
// 硬要求（方案 §6.5/§13-4）：10s 缓存 + 同键单飞。实现：
// - **按 runtime 指针隔离**：多 AppState / 嵌入场景不串用快照（不同
//   runtime 的 namespace/容器集不同，串用会让 B 看到 A 的数据）；
// - **tokio 异步锁单飞**：miss 的并发请求在采集锁上排队，第二个进来时
//   先重查缓存（双检）——上游只打一次；
// - **固定按上限采集事件**（MAX_EVENTS），展示时按 events_limit 截断：
//   20→50→20 的参数交替不再互相驱逐缓存；
// - 过滤（search/身份）在快照上做内存运算，不进缓存键。
//
// 锁中毒处理：poisoned 视为"无缓存可用"直接采集——缓存是纯优化，
// 不能因 poison 让观测接口 5xx（生产代码不留 panic 路径）。

/// 采集时的固定事件条数上限（= events_limit 的 clamp 上限）。
const MAX_EVENTS: usize = 50;

#[derive(Clone)]
struct OverviewCache {
    snapshot: PodOverviewSnapshot,
    ts: std::time::Instant,
}

#[derive(Default)]
struct CacheCell {
    cached: Mutex<Option<OverviewCache>>,
    /// 采集串行化（单飞）。
    inflight: tokio::sync::Mutex<()>,
}

static CACHES: LazyLock<Mutex<HashMap<usize, Arc<CacheCell>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 缓存 TTL。metrics-server 本身 15~60s 粒度，10s 足够"新鲜"；
/// 面板轮询在这层被吸收，不会打穿到 k8s API。
const CACHE_TTL_SECS: u64 = 10;

/// 读缓存（短临界区：Clone 后立即释放）。中毒 → None（当没缓存）。
fn read_cache(cell: &Arc<CacheCell>) -> Option<OverviewCache> {
    let guard = cell.cached.lock().ok()?;
    guard.clone()
}

async fn snapshot_with_cache(
    state: &Arc<AppState>,
) -> Result<(PodOverviewSnapshot, u64), AppError> {
    // runtime 身份 = Arc 指针（同一 runtime 的所有 AppState 共享同一 Arc）
    let runtime = state.runtime().clone();
    // fat pointer（dyn）先转 thin：取数据指针地址做键
    let key = Arc::as_ptr(&runtime) as *const () as usize;
    let cell = {
        let mut caches = match CACHES.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(), // 缓存簿记中毒 ≠ 5xx
        };
        caches
            .entry(key)
            .or_insert_with(|| Arc::new(CacheCell::default()))
            .clone()
    };

    // 快路径：命中新鲜缓存直接返回
    if let Some(cached) = read_cache(&cell)
        && cached.ts.elapsed().as_secs() < CACHE_TTL_SECS
    {
        return Ok((cached.snapshot, cached.ts.elapsed().as_secs()));
    }

    // 单飞：采集锁内双检（并发 miss 只打一次上游）
    let _inflight = cell.inflight.lock().await;
    if let Some(cached) = read_cache(&cell)
        && cached.ts.elapsed().as_secs() < CACHE_TTL_SECS
    {
        return Ok((cached.snapshot, cached.ts.elapsed().as_secs()));
    }
    let snapshot = runtime
        .observe_namespace_pods(MAX_EVENTS)
        .await
        .map_err(|e| {
            error!(event = "pod_overview.observe_failed", error = %e, "observe failed");
            // R12：列表失败 = 503 语义（服务/依赖暂不可用），带结构化 stage
            AppError::from_code(shared_types::error_codes::ERR_SERVICE_UNAVAILABLE)
                .with_error_detail(shared_types::ErrorDetail::new(
                    shared_types::error_codes::ERR_SERVICE_UNAVAILABLE,
                    "pod.overview.list",
                    format!("pod overview observation failed: {e}"),
                ))
        })?;
    let entry = OverviewCache {
        snapshot: snapshot.clone(),
        ts: std::time::Instant::now(),
    };
    if let Ok(mut guard) = cell.cached.lock() {
        *guard = Some(entry);
    }
    Ok((snapshot, 0))
}

/// 隐藏的内部 sidecar（与方案 §13 决策 2 一致）。
const SIDECAR_PREFIXES: &[&str] = &["log-shipper", "log-collector"];

/// 查询命名空间级 Pod 概览。
#[utoipa::path(
    get,
    path = "/computer/pod/overview",
    tag = "overview",
    operation_id = "computer_pod_overview",
    params(PodOverviewQuery),
    responses(
        (status = 200, description = "命名空间内全部 Pod 的规格/健康/事件概览（分区失败以 degraded 标注；10s 缓存）", body = HttpResult<PodOverviewResponse>),
        (status = 503, description = "Pod 列表本身失败（没有列表这个查询没有意义）", body = HttpResult<String>)
    )
)]
#[instrument(skip(state))]
pub async fn pod_overview(
    State(state): State<Arc<AppState>>,
    Query(params): Query<PodOverviewQuery>,
) -> Result<HttpResult<PodOverviewResponse>, AppError> {
    let locale = shared_types::current_request_locale();
    let events_limit = params.events_limit.unwrap_or(20).clamp(1, 50) as usize;
    let limit = params.limit.unwrap_or(500).clamp(1, 1000) as usize;
    let include_sidecars = params.include_sidecars.unwrap_or(false);

    let (snapshot, cache_age) = snapshot_with_cache(&state).await?;

    // R12：非法 service_type 必须明确拒绝——静默变成"无过滤"会把输入错误
    // 伪装成"返回全集"，调用方以为筛选命中了
    let parsed_service_type = match params.service_type.as_deref() {
        None => None,
        Some(raw) => match raw.parse() {
            Ok(value) => Some(value),
            Err(_) => {
                warn!(
                    event = "pod_overview.invalid_service_type",
                    value = %raw,
                    "invalid service_type filter"
                );
                let mut response = HttpResult::error_with_locale(
                    shared_types::error_codes::ERR_VALIDATION,
                    locale,
                );
                response.error_detail = Some(
                    shared_types::ErrorDetail::new(
                        shared_types::error_codes::ERR_VALIDATION,
                        "pod.overview.resolve",
                        format!("invalid service_type {raw:?}"),
                    )
                    .localized(locale),
                );
                return Ok(response);
            }
        },
    };
    // 过滤（内存，快照上）
    let filter = PodOverviewFilter {
        search: params.search.as_deref().map(str::to_string),
        service_type: parsed_service_type,
        user_id: params.user_id.clone(),
        app_id: params.app_id.clone(),
        project_id: params.project_id.clone(),
        label_selector: None, // P1：label 过滤在采集端做（透传），后续按需加
    };
    let mut pods: Vec<_> = apply_filter(&snapshot, &filter)
        .into_iter()
        .cloned()
        .collect();
    // 展示截断：缓存固定采集 MAX_EVENTS 条，按请求的 events_limit 截断
    // （R10：展示参数不再影响采集/缓存）
    for pod in &mut pods {
        pod.events.truncate(events_limit);
    }

    // sidecar 隐藏（默认）
    if !include_sidecars {
        for pod in &mut pods {
            pod.spec
                .containers
                .retain(|c| !SIDECAR_PREFIXES.iter().any(|p| c.name.starts_with(p)));
        }
    }

    // 汇总在全量快照上算（不受过滤影响）
    let summary = summarize(&snapshot);

    // 排序：异常优先（未就绪 > 有 OOM > 有重启 > 名字），见方案 §6.4
    pods.sort_by(|a, b| {
        // 异常优先 = 严重度**降序**（bool 里 false < true，所以 (b).cmp(&(a))
        // 会把 !ready=true / oom=true 的排前面）
        let severity = |p: &container_runtime_api::pod_overview::PodOverviewItem| {
            (
                !p.ready,
                p.health.oom_killed_last,
                p.health.restart_count > 0,
            )
        };
        severity(b)
            .cmp(&severity(a))
            .then_with(|| b.health.restart_count.cmp(&a.health.restart_count))
            .then_with(|| a.name.cmp(&b.name))
    });
    pods.truncate(limit);

    // R5：后端形态由**采集方声明**（快照携带）——不能用"命名域方法是否
    // 实现"推断：docker 运行时也实现了该方法（返回 docker:<addr>），
    // 据此推断会把 docker 标成 kubernetes。
    let source = if snapshot.source.is_empty() {
        "unknown".to_string()
    } else {
        snapshot.source.clone()
    };
    let namespace = snapshot.namespace.clone();

    info!(
        event = "pod_overview.served",
        total = summary.pods,
        returned = pods.len(),
        cache_age_seconds = cache_age,
        degraded = snapshot.degraded.len(),
        "pod overview served"
    );

    Ok(HttpResult::success(PodOverviewResponse {
        source: source.to_string(),
        namespace,
        generated_at: chrono::Utc::now(),
        cache_age_seconds: cache_age,
        summary,
        degraded: snapshot.degraded.clone(),
        pods,
    }))
}

/// 全量汇总。
fn summarize(snapshot: &PodOverviewSnapshot) -> PodOverviewSummary {
    let mut summary = PodOverviewSummary::default();
    for pod in &snapshot.pods {
        summary.pods += 1;
        if pod.phase == "running" || pod.phase == "Running" {
            summary.running += 1;
        }
        if !pod.ready {
            summary.not_ready += 1;
        }
        if pod.health.restart_count > 0 {
            summary.restarted += 1;
        }
        if pod.health.oom_killed_last {
            summary.oom_killed_last += 1;
        }
        if pod.events.iter().any(|e| e.event_type == "Warning") {
            summary.with_warning_events += 1;
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(
        name: &str,
        ready: bool,
        oom: bool,
        restarts: u32,
    ) -> container_runtime_api::pod_overview::PodOverviewItem {
        container_runtime_api::pod_overview::PodOverviewItem {
            name: name.to_string(),
            workload: None,
            identity: None,
            phase: "Running".to_string(),
            ready,
            node: None,
            ip: None,
            created_at: None,
            spec: Default::default(),
            health: container_runtime_api::pod_overview::PodHealth {
                restart_count: restarts,
                oom_killed_last: oom,
                ..Default::default()
            },
            events: Vec::new(),
        }
    }

    /// 汇总必须反映全量，而不是过滤后的子集。
    #[test]
    fn summary_counts_full_snapshot() {
        let snapshot = PodOverviewSnapshot {
            pods: vec![
                item("a", true, false, 0),
                item("b", false, false, 3),
                item("c", true, true, 1),
            ],
            degraded: Vec::new(),
            source: "kubernetes".to_string(),
            namespace: Some("test".to_string()),
        };
        let s = summarize(&snapshot);
        assert_eq!(s.pods, 3);
        assert_eq!(s.running, 3);
        assert_eq!(s.not_ready, 1);
        assert_eq!(s.restarted, 2);
        assert_eq!(s.oom_killed_last, 1);
    }

    /// 排序：未就绪 > OOM > 有重启；同级按重启数降序、名字升序。
    #[test]
    fn problem_pods_sort_first() {
        let mut pods = [
            item("z-healthy", true, false, 0),
            item("m-notready", false, false, 0),
            item("a-oom", true, true, 0),
            item("b-restarts", true, false, 9),
        ];
        pods.sort_by(|a, b| {
            let severity = |p: &container_runtime_api::pod_overview::PodOverviewItem| {
                (
                    !p.ready,
                    p.health.oom_killed_last,
                    p.health.restart_count > 0,
                )
            };
            severity(b)
                .cmp(&severity(a))
                .then_with(|| b.health.restart_count.cmp(&a.health.restart_count))
                .then_with(|| a.name.cmp(&b.name))
        });
        let names: Vec<_> = pods.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["m-notready", "a-oom", "b-restarts", "z-healthy"]
        );
    }

    /// sidecar 默认隐藏，include_sidecars=true 时保留。
    #[test]
    fn sidecars_hidden_by_default() {
        let mut pod = item("x", true, false, 0);
        pod.spec.containers = vec![
            container_runtime_api::pod_overview::ContainerResourcesSpec {
                name: "agent".into(),
                ..Default::default()
            },
            container_runtime_api::pod_overview::ContainerResourcesSpec {
                name: "log-collector".into(),
                ..Default::default()
            },
            container_runtime_api::pod_overview::ContainerResourcesSpec {
                name: "log-shipper".into(),
                ..Default::default()
            },
        ];
        let mut pods = [pod];
        for pod in &mut pods {
            pod.spec
                .containers
                .retain(|c| !SIDECAR_PREFIXES.iter().any(|p| c.name.starts_with(p)));
        }
        assert_eq!(pods[0].spec.containers.len(), 1);
        assert_eq!(pods[0].spec.containers[0].name, "agent");
    }

    /// events_limit 边界：0/超大值被 clamp。
    #[test]
    fn events_limit_clamped() {
        assert_eq!(
            PodOverviewQuery {
                events_limit: Some(0),
                ..Default::default()
            }
            .events_limit
            .unwrap_or(20)
            .clamp(1, 50),
            1
        );
        assert_eq!(
            PodOverviewQuery {
                events_limit: Some(999),
                ..Default::default()
            }
            .events_limit
            .unwrap_or(20)
            .clamp(1, 50),
            50
        );
    }

    /// degraded 为空与"没有事件"必须可区分——这条契约靠类型（degraded 数组）保证。
    #[test]
    fn degraded_distinguishes_missing_from_empty() {
        let snapshot = PodOverviewSnapshot {
            pods: vec![item("x", true, false, 0)],
            degraded: vec![container_runtime_api::pod_overview::PodOverviewDegraded {
                section: "events".into(),
                reason: "events_list_failed".into(),
                detail: None,
            }],
            ..Default::default()
        };
        assert_eq!(snapshot.degraded.len(), 1);
        assert!(snapshot.pods[0].events.is_empty(), "降级时事件为空数组");
    }
}
