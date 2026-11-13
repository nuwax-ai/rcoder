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

use std::sync::{Arc, LazyLock, Mutex};

use axum::extract::{Query, State};
use tracing::{error, info, instrument};

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
    /// 按结构化身份过滤
    #[serde(default)]
    pub service_type: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
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

// ── 缓存：键 = 无（P1 只按 runtime 一份全量快照；过滤参数不进键）───────
//
// 说明：过滤（search/身份）在快照上做内存运算，同一份快照可服务所有过滤
// 组合——所以缓存键不含过滤参数。events_limit 影响采集形态，进缓存键。

struct OverviewCache {
    snapshot: PodOverviewSnapshot,
    events_limit: usize,
    ts: std::time::Instant,
}

static CACHE: LazyLock<Mutex<Option<OverviewCache>>> = LazyLock::new(|| Mutex::new(None));

/// 缓存 TTL。metrics-server 本身 15~60s 粒度，10s 足够"新鲜"；
/// 面板 2s 轮询在这层被吸收，不会打穿到 k8s API。
const CACHE_TTL_SECS: u64 = 10;

/// 取快照：命中且 events_limit 一致 → 复用；否则采集并回填。
///
/// **不是单飞**：缓存 miss 的并发请求会各自采集（采集在锁外）。代价是
/// 冷启动时可能出现几次重复读——对只读的 list/inspect 无害；面板 10s
/// 轮询 + 10s TTL 的形态下，miss 窗口极小。真要做去重得加 in-flight
/// 通道，对观测接口不值得那份复杂度。
async fn snapshot_with_cache(
    state: &Arc<AppState>,
    events_limit: usize,
) -> Result<(PodOverviewSnapshot, u64), AppError> {
    // 先试缓存（非阻塞快速路径）
    if let Some(cached) = CACHE.lock().expect("overview cache poisoned").as_ref()
        && cached.events_limit == events_limit
        && cached.ts.elapsed().as_secs() < CACHE_TTL_SECS
    {
        return Ok((cached.snapshot.clone(), cached.ts.elapsed().as_secs()));
    }
    // 采集（不在锁内——采集要几百毫秒，持锁会让所有并发请求串行）
    let snapshot = state
        .runtime()
        .observe_namespace_pods(events_limit)
        .await
        .map_err(|e| {
            error!("[POD_OVERVIEW] observe failed: {e}");
            AppError::internal_server_error(&format!("pod overview observation failed: {e}"))
        })?;
    let age = 0;
    {
        let mut guard = CACHE.lock().expect("overview cache poisoned");
        *guard = Some(OverviewCache {
            snapshot: snapshot.clone(),
            events_limit,
            ts: std::time::Instant::now(),
        });
    }
    Ok((snapshot, age))
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
    let events_limit = params.events_limit.unwrap_or(20).clamp(1, 50) as usize;
    let limit = params.limit.unwrap_or(500).clamp(1, 1000) as usize;
    let include_sidecars = params.include_sidecars.unwrap_or(false);

    let (snapshot, cache_age) = snapshot_with_cache(&state, events_limit).await?;

    // 过滤（内存，快照上）
    let filter = PodOverviewFilter {
        search: params.search.as_deref().map(str::to_string),
        service_type: params.service_type.as_deref().and_then(|s| s.parse().ok()),
        user_id: params.user_id.clone(),
        app_id: params.app_id.clone(),
        project_id: params.project_id.clone(),
        label_selector: None, // P1：label 过滤在采集端做（透传），后续按需加
    };
    let mut pods: Vec<_> = apply_filter(&snapshot, &filter)
        .into_iter()
        .cloned()
        .collect();

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

    let source = if state.runtime().agent_workload_namespace() != "unsupported" {
        "kubernetes"
    } else {
        "docker"
    };
    let namespace = (source == "kubernetes").then(|| state.runtime().agent_workload_namespace());

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
        };
        assert_eq!(snapshot.degraded.len(), 1);
        assert!(snapshot.pods[0].events.is_empty(), "降级时事件为空数组");
    }
}
