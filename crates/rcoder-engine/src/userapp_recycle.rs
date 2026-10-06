//! Userapp 闲置自动回收扫描器（后台定时任务，leader 副本运行）。
//!
//! 周期枚举所有 Running Userapp,比对 `last_accessed_at` 与阈值,闲置超阈值 → `stop_app`(scale0,
//! 不删 PVC/Service/路由)。付费 app(`recycle_enabled=false` 注解)opt-out 跳过;进行中的唤醒跳过;
//! 龄期 < protection 跳过；从未访问时，持续 NotReady 超过保护和闲置阈值才回收。
//!
//! 闲置信号来自 pingora 热路径 `AppAccessTracker::touch`(经 [`AppActivityRegistry`] 维护)。
//! 多副本下 touch 分摊在各副本内存，leader 每轮扫描从 PG 影子行合并最新值（`merge_accessed`），
//! 防活跃流量全落其他副本时误回收。回收 = scale-to-zero,数据零风险;唤醒由 pingora
//! `request_filter` 的 wake-on-traffic 负责。
//!
//! 回收判定逻辑抽成纯函数 [`decide_recycle`](见模块底),不依赖 AppState/K8s,便于单测覆盖所有分支。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use shared_types::{AppWakeControl, UserAppLifecycleStore};
use tracing::{debug, info, warn};

use crate::app_state::AppState;

#[cfg(all(test, feature = "userapp-turso"))]
#[path = "userapp_recycle/expired_wake_scan_tests.rs"]
mod expired_wake_scan_tests;

/// 扫描器运行期配置(秒 → Duration,由 background_tasks 从 AppConfig 装配)
pub(crate) struct UserAppRecycleRuntimeConfig {
    /// 闲置阈值(秒;per-app 注解可覆盖)
    pub idle_timeout: Duration,
    /// 扫描间隔
    pub scan_interval: Duration,
    /// 新建 app 最小保护期(龄期小于此值不回收)
    pub protection: Duration,
}

pub(crate) struct UserAppRecycleScanner {
    config: UserAppRecycleRuntimeConfig,
    state: Arc<AppState>,
    /// Only consecutive successful observations of the same managed workload.
    not_ready_seen: HashMap<String, UnhealthyObservation>,
}

impl UserAppRecycleScanner {
    pub(crate) fn new(config: UserAppRecycleRuntimeConfig, state: Arc<AppState>) -> Self {
        Self {
            config,
            state,
            not_ready_seen: Default::default(),
        }
    }

    pub async fn run(mut self, mut shutdown_rx: tokio::sync::broadcast::Receiver<()>) {
        info!(
            "[USERAPP_RECYCLE] scanner started (interval={:?}, idle_timeout={:?}, protection={:?})",
            self.config.scan_interval, self.config.idle_timeout, self.config.protection
        );
        let mut interval = tokio::time::interval(self.config.scan_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await; // 消耗首次立即 tick(给启动 grace;Running app 已被 rebuild_stopped_apps 种 last_accessed=now)
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match self.do_scan().await {
                        Ok(n) => debug!("[USERAPP_RECYCLE] scan done: {} recycled this tick", n),
                        Err(e) => warn!("[USERAPP_RECYCLE] scan failed: {}", e),
                    }
                }
                _ = shutdown_rx.recv() => {
                    info!("[USERAPP_RECYCLE] shutdown");
                    break;
                }
            }
        }
    }

    /// 单轮扫描;返回本轮回收的 app 数。整轮失败向上传播(调用方 warn);单 app 失败隔离不影响其他。
    async fn do_scan(&mut self) -> anyhow::Result<usize> {
        // A failed/partial scan must break continuity, including identity lookup failures.
        let previous = std::mem::take(&mut self.not_ready_seen);
        let mut observations = HashMap::new();
        let apps = self
            .state
            .app_service
            .list_all_app_runtimes()
            .await
            .map_err(|e| anyhow::anyhow!("list_all_app_runtimes: {e}"))?;
        // 多副本：touch 分摊在各副本内存（leader 眼里只有落在本副本的流量），
        // 跨副本合并经 PG 影子行——各副本 5s flusher 落库，leader 每轮扫描
        // 读一次最新值取 max（防活跃流量全落其他副本时误回收）。非 PG 模式
        // 退化为纯内存判定（单副本部署，无跨副本问题）。
        let pg_accessed: HashMap<(String, String), DateTime<Utc>> = match self
            .state
            .activity
            .persistence()
        {
            Some(p) => match p.load_all().await {
                Ok(rows) => rows
                    .into_iter()
                    .filter_map(|r| r.last_accessed.map(|t| ((r.app_id, r.lifecycle_id), t)))
                    .collect(),
                Err(e) => {
                    return Err(e.context("Activity snapshot unavailable; skip recycling instead of ignoring peer traffic"));
                }
            },
            None => Default::default(),
        };
        // 闲置时长按 wall-clock 计算（last_accessed 为 DateTime，可跨重启持久化）；
        // 负值（时钟回拨）按 0 处理
        let now = Utc::now();
        let mut recycled = 0usize;

        for app in &apps {
            let identity = match self.state.userapp_store.get_application(&app.app_id).await {
                Ok(Some(identity)) => identity,
                Ok(None) => continue,
                Err(error) => {
                    warn!(app_id = %app.app_id, %error, "Recycle identity lookup failed");
                    continue;
                }
            };
            if identity.state != shared_types::UserAppLifecycleState::Active {
                continue;
            }
            if !self.state.activity.bind_lifecycle(
                &app.app_id,
                &identity.lifecycle_id,
                identity.lifecycle_epoch,
            ) {
                continue;
            }
            let age = app.created_at.as_deref().and_then(age_of);
            // PG 较新时 merge_accessed 会回写内存（保 try_begin_recycle 的 epoch 复核）
            let last_accessed =
                match pg_accessed.get(&(app.app_id.clone(), identity.lifecycle_id.clone())) {
                    Some(pg_t) => self.state.activity.merge_lifecycle_accessed(
                        &app.app_id,
                        &identity.lifecycle_id,
                        identity.lifecycle_epoch,
                        *pg_t,
                    ),
                    None => self.state.activity.last_accessed_at(&app.app_id),
                };
            let idle =
                last_accessed.map(|t| now.signed_duration_since(t).to_std().unwrap_or_default());
            let unhealthy = (app.replicas > 0 && app.ready_replicas == 0).then(|| {
                UnhealthyObservation::observe(
                    previous.get(&app.app_id),
                    ObservationIdentity {
                        lifecycle_id: identity.lifecycle_id.clone(),
                        created_at: app.created_at.clone(),
                    },
                    tokio::time::Instant::now(),
                    now,
                )
            });
            let unhealthy_for = unhealthy.as_ref().map(|seen| seen.since.elapsed());
            if let Some(seen) = unhealthy.as_ref() {
                observations.insert(app.app_id.clone(), seen.clone());
            }
            let input = RecycleEvalInput {
                replicas: app.replicas,
                ready_replicas: app.ready_replicas,
                unhealthy_for,
                recycle_enabled: app.recycle_enabled,
                is_waking: self.state.activity.is_waking(&app.app_id),
                age,
                idle,
                per_app_idle: app.idle_timeout_seconds.map(Duration::from_secs),
            };
            let decision = decide_recycle(&input, &self.config);
            let app_id = &app.app_id;
            match decision {
                RecycleDecision::Recycle => {
                    // Never-accessed workloads need a local grace baseline for the
                    // existing compare-before-recycle guard. Merge (max), never overwrite
                    // a real access arriving between the scan and this transition.
                    let Some(observed_access) = recycle_access_baseline(
                        &self.state.activity,
                        &identity,
                        last_accessed,
                        unhealthy.as_ref().map(|seen| seen.baseline),
                    ) else {
                        continue;
                    };
                    let rechecked = RecycleEvalInput {
                        idle: Some(
                            Utc::now()
                                .signed_duration_since(observed_access)
                                .to_std()
                                .unwrap_or_default(),
                        ),
                        ..input
                    };
                    if decide_recycle(&rechecked, &self.config) != RecycleDecision::Recycle {
                        continue;
                    }
                    // Close only acknowledged, expired read-only wake observation.
                    // Deploy/delete/PG preparation and unknown runtime writes stay intact.
                    let recovery = tokio::time::timeout(
                        Duration::from_secs(30),
                        recover_expired_wake_observation(
                            self.state.userapp_store.as_ref(),
                            self.state.app_service.as_ref(),
                            &identity,
                            Utc::now(),
                            self.state.activity.wake_timeout(),
                        ),
                    )
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result);
                    match recovery {
                        Err(error) => {
                            warn!(%app_id, %error, "Recycle wake recovery incomplete");
                            continue;
                        }
                        Ok(false)
                            if identity.active_operations.prod.is_some()
                                || identity.active_operations.application.is_some() =>
                        {
                            debug!(%app_id, "Skip background recycling while ordinary operation is active");
                            continue;
                        }
                        _ => {}
                    }
                    // 原子登记回收过渡并复核访问 epoch。新请求若已 touch，本次回收失效；
                    // 若在登记后到达，请求会等 scale0 完成后再唤醒。
                    let Some(_transition) = self
                        .state
                        .activity
                        .try_begin_recycle(app_id, observed_access)
                    else {
                        debug!("[USERAPP_RECYCLE] skip {app_id}: access changed before recycle");
                        continue;
                    };
                    // 命中：以“允许流量唤醒”的语义回收到 scale0。每 app 错误隔离。
                    if let Err(e) = self.state.app_service.recycle_app(app_id).await {
                        warn!("[USERAPP_RECYCLE] recycle_app failed app_id={app_id}: {e}");
                    } else {
                        info!(
                            "[USERAPP_RECYCLE] recycled idle app: {app_id} (idle={:?})",
                            idle
                        );
                        recycled += 1;
                    }
                }
                RecycleDecision::Skip(reason) => {
                    debug!("[USERAPP_RECYCLE] skip {app_id}: {reason}");
                }
            }
        }
        self.not_ready_seen = observations;
        Ok(recycled)
    }
}

/// Discovery fingerprints only reset the grace clock; runtime writes still
/// capture and validate their own physical UID under the operation lease.
/// Container started_at changes on every crash restart; using it here would
/// indefinitely renew a CrashLoop's grace. Health instance names are synthetic.
#[derive(Clone, PartialEq, Eq)]
struct ObservationIdentity {
    lifecycle_id: String,
    created_at: Option<String>,
}

#[derive(Clone)]
struct UnhealthyObservation {
    identity: ObservationIdentity,
    since: tokio::time::Instant,
    baseline: DateTime<Utc>,
}

impl UnhealthyObservation {
    fn observe(
        previous: Option<&Self>,
        identity: ObservationIdentity,
        now: tokio::time::Instant,
        wall: DateTime<Utc>,
    ) -> Self {
        match previous.filter(|old| old.identity == identity) {
            Some(old) => old.clone(),
            None => Self {
                identity,
                since: now,
                baseline: wall,
            },
        }
    }
}

fn recycle_access_baseline(
    activity: &app_manager::AppActivityRegistry,
    identity: &shared_types::UserAppLifecycleRecord,
    accessed: Option<DateTime<Utc>>,
    unhealthy_since: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    let baseline = accessed.or(unhealthy_since)?;
    activity.merge_lifecycle_accessed(
        &identity.app_id,
        &identity.lifecycle_id,
        identity.lifecycle_epoch,
        baseline,
    )
}

/// This is not a generic operation TTL. Only traffic-wake's durable acknowledged
/// write boundary permits closing its read-only observation after its budget.
pub(crate) async fn recover_expired_wake_observation(
    store: &dyn UserAppLifecycleStore,
    service: &dyn app_manager::AppServiceTrait,
    app: &shared_types::UserAppLifecycleRecord,
    now: DateTime<Utc>,
    wake_budget: Duration,
) -> anyhow::Result<bool> {
    use shared_types::{UserAppOperationKind as Kind, UserAppOperationState as State};
    if app.active_operations.application.is_some() {
        return Ok(false);
    }
    let Some(id) = app.active_operations.prod.as_deref() else {
        return Ok(false);
    };
    let Some(mut operation) = store.get_operation(&app.app_id, id).await? else {
        return Ok(false);
    };
    let grace =
        Duration::from_secs(2 * 60 * 60).max(wake_budget.saturating_add(Duration::from_secs(60)));
    if operation.lifecycle_id != app.lifecycle_id
        || operation.kind != Kind::Start
        || operation.scope != shared_types::UserAppOperationScope::Prod
        || !matches!(operation.state, State::Running | State::RecoveryRequired)
        || operation.step != "traffic_wake_observing"
        || operation.checkpoint.get("start_write_acknowledged")
            != Some(&serde_json::Value::Bool(true))
        || now
            .signed_duration_since(operation.created_at)
            .to_std()
            .unwrap_or_default()
            < grace
    {
        return Ok(false);
    }
    if store
        .operation_deadline(&app.app_id, id)
        .await?
        .is_some_and(|deadline| deadline > now.timestamp_millis())
    {
        return Ok(false);
    }
    // Use the same evidence predicate as durable wake finalization, before even
    // changing Running to RecoveryRequired. A new revision wins over this snapshot.
    let target: shared_types::UserAppMutationTarget = serde_json::from_value(
        operation
            .checkpoint
            .get("target")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Wake target missing"))?,
    )?;
    anyhow::ensure!(
        target.context.app_id == operation.app_id
            && target.context.lifecycle_id == operation.lifecycle_id
            && target.context.operation_id == operation.operation_id
            && Some(&target.context.executor_id) == operation.executor_id.as_ref()
            && target.context.request_fingerprint == operation.request_fingerprint,
        "Wake evidence identity mismatch"
    );
    store
        .check_compute_access(
            &app.app_id,
            shared_types::UserAppOperationScope::Prod,
            false,
        )
        .await?;
    if operation.state == State::Running {
        operation = store
            .advance(&shared_types::UserAppOperationProgress {
                app_id: operation.app_id.clone(),
                lifecycle_id: operation.lifecycle_id.clone(),
                operation_id: operation.operation_id.clone(),
                expected_revision: operation.revision,
                executor_id: target.context.executor_id,
                state: State::RecoveryRequired,
                step: operation.step.clone(),
                checkpoint: operation.checkpoint.clone(),
                error_code: Some("ERR_WAKE_OBSERVATION_EXPIRED".into()),
                error_message: Some(
                    "Acknowledged traffic wake observation exceeded its recovery grace".into(),
                ),
            })
            .await?;
    }
    // Existing full-record CAS finalizes Failed, then releases only the original
    // operation-bound lease. A crash between those steps is handled by the
    // existing terminal-lease scanner; no replacement Start is issued here.
    service
        .retry_control_operation(
            &app.app_id,
            id,
            shared_types::UserAppRetryRequest {
                lifecycle_id: operation.lifecycle_id,
                expected_revision: operation.revision,
            },
        )
        .await?;
    info!(app_id = %app.app_id, operation_id = %id, "Expired acknowledged wake observation reconciled");
    Ok(true)
}

/// RFC3339 创建时间 → 至今的龄期(future/解析失败 → None)
fn age_of(created_at: &str) -> Option<Duration> {
    let t = DateTime::parse_from_rfc3339(created_at).ok()?;
    let diff = Utc::now().signed_duration_since(t);
    diff.to_std().ok()
}

/// 回收判定结果
#[derive(Debug, PartialEq, Eq)]
enum RecycleDecision {
    Recycle,
    Skip(SkipReason),
}

#[derive(Debug, PartialEq, Eq)]
enum SkipReason {
    NotRunning,
    OptOut,
    WakeInFlight,
    WithinProtection,
    NeverAccessed,
    BelowThreshold,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRunning => write!(f, "not running"),
            Self::OptOut => write!(f, "opt-out (paid)"),
            Self::WakeInFlight => write!(f, "wake in flight"),
            Self::WithinProtection => write!(f, "within protection"),
            Self::NeverAccessed => write!(f, "never accessed (grace)"),
            Self::BelowThreshold => write!(f, "below idle threshold"),
        }
    }
}

/// 单 app 的回收判定输入（扫描器从 `AppRuntimeInfo` + activity registry 装配）。
#[derive(Default, Clone)]
struct RecycleEvalInput {
    replicas: i32,
    /// Observed ready replicas; zero alone does not establish a dead executor.
    ready_replicas: i32,
    /// Continuous NotReady duration for the same managed workload.
    unhealthy_for: Option<Duration>,
    /// absent / Some(true) = 可回收；Some(false) = 付费永不回收
    recycle_enabled: Option<bool>,
    is_waking: bool,
    /// `Some(创建至今)`；`None`=未知（无法判 protection → 放行后续检查）
    age: Option<Duration>,
    /// `Some(最近访问至今)`；`None`=从未被 HTTP 访问 → grace 跳过
    idle: Option<Duration>,
    /// per-app 注解覆盖；`None`=用全局 `cfg.idle_timeout`
    per_app_idle: Option<Duration>,
}

/// 纯函数：给定 app 状态 + 配置，判定是否应回收。提取自 `do_scan` 便于单测覆盖所有跳过分支。
///
/// 判定顺序(短路):非 Running → 付费 opt-out → 唤醒中 → protection 龄期 → 从未访问 grace → 闲置阈值。
fn decide_recycle(input: &RecycleEvalInput, cfg: &UserAppRecycleRuntimeConfig) -> RecycleDecision {
    if input.replicas <= 0 {
        return RecycleDecision::Skip(SkipReason::NotRunning);
    }
    // absent / Some(true) = 可回收(免费默认);Some(false) = 付费永不回收
    if input.recycle_enabled == Some(false) {
        return RecycleDecision::Skip(SkipReason::OptOut);
    }
    if input.is_waking {
        return RecycleDecision::Skip(SkipReason::WakeInFlight);
    }
    if let Some(age) = input.age
        && age < cfg.protection
    {
        return RecycleDecision::Skip(SkipReason::WithinProtection);
    }
    let threshold = input.per_app_idle.unwrap_or(cfg.idle_timeout);
    let idle = match input.idle {
        Some(idle) => idle,
        None => match input.unhealthy_for {
            Some(duration)
                if input.ready_replicas == 0
                    && duration >= cfg.scan_interval.max(cfg.protection) =>
            {
                duration
            }
            _ => return RecycleDecision::Skip(SkipReason::NeverAccessed),
        },
    };
    if idle < threshold {
        return RecycleDecision::Skip(SkipReason::BelowThreshold);
    }
    RecycleDecision::Recycle
}

/// 启动回收扫描后台任务(由 background_tasks 在 enabled 时调用)
pub(crate) async fn start_userapp_recycle_task(
    config: UserAppRecycleRuntimeConfig,
    state: Arc<AppState>,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let scanner = UserAppRecycleScanner::new(config, state);
    let shutdown_rx = shutdown_tx.subscribe();
    Ok(tokio::task::spawn(scanner.run(shutdown_rx)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> UserAppRecycleRuntimeConfig {
        UserAppRecycleRuntimeConfig {
            idle_timeout: Duration::from_secs(
                crate::config::UserAppRecycleConfig::default().idle_timeout_seconds,
            ),
            scan_interval: Duration::from_secs(3600),
            protection: Duration::from_secs(300),
        }
    }

    // ---- decide_recycle: 全部分支(named-field 输入,..Default 聚焦被测字段) ----

    #[test]
    fn decide_recycles_idle_running_app() {
        // Running + 可回收 + 龄期足够 + idle 超全局阈值 → Recycle
        let d = decide_recycle(
            &RecycleEvalInput {
                replicas: 1,
                age: Some(Duration::from_secs(7200)),
                idle: Some(Duration::from_secs(3600)),
                ..Default::default()
            },
            &cfg(),
        );
        assert_eq!(d, RecycleDecision::Recycle);
    }

    #[test]
    fn decide_skips_not_running() {
        let d = decide_recycle(
            &RecycleEvalInput {
                replicas: 0,
                ..Default::default()
            },
            &cfg(),
        );
        assert_eq!(d, RecycleDecision::Skip(SkipReason::NotRunning));
    }

    #[test]
    fn decide_skips_paid_opt_out() {
        let d = decide_recycle(
            &RecycleEvalInput {
                replicas: 1,
                recycle_enabled: Some(false),
                age: Some(Duration::from_secs(1000)),
                idle: Some(Duration::from_secs(500_000)),
                ..Default::default()
            },
            &cfg(),
        );
        assert_eq!(d, RecycleDecision::Skip(SkipReason::OptOut));
    }

    #[test]
    fn decide_skips_wake_in_flight() {
        let d = decide_recycle(
            &RecycleEvalInput {
                replicas: 1,
                is_waking: true,
                age: Some(Duration::from_secs(1000)),
                idle: Some(Duration::from_secs(500_000)),
                ..Default::default()
            },
            &cfg(),
        );
        assert_eq!(d, RecycleDecision::Skip(SkipReason::WakeInFlight));
    }

    #[test]
    fn decide_skips_within_protection() {
        // age=10s < protection=300s
        let d = decide_recycle(
            &RecycleEvalInput {
                replicas: 1,
                age: Some(Duration::from_secs(10)),
                idle: Some(Duration::from_secs(500_000)),
                ..Default::default()
            },
            &cfg(),
        );
        assert_eq!(d, RecycleDecision::Skip(SkipReason::WithinProtection));
    }

    #[test]
    fn decide_skips_never_accessed() {
        // idle=None → grace(刚建还没流量)
        let d = decide_recycle(
            &RecycleEvalInput {
                replicas: 1,
                age: Some(Duration::from_secs(1000)),
                ..Default::default() // idle 默认 None
            },
            &cfg(),
        );
        assert_eq!(d, RecycleDecision::Skip(SkipReason::NeverAccessed));
    }

    #[test]
    fn decide_skips_below_threshold() {
        // idle=3599s < 全局阈值 3600s
        let d = decide_recycle(
            &RecycleEvalInput {
                replicas: 1,
                age: Some(Duration::from_secs(7200)),
                idle: Some(Duration::from_secs(3599)),
                ..Default::default()
            },
            &cfg(),
        );
        assert_eq!(d, RecycleDecision::Skip(SkipReason::BelowThreshold));
    }

    #[test]
    fn decide_per_app_threshold_overrides_global() {
        // per-app 阈值=60s;idle=100s > 60 → Recycle(即便全局 3600 本会跳过)
        let d = decide_recycle(
            &RecycleEvalInput {
                replicas: 1,
                age: Some(Duration::from_secs(1000)),
                idle: Some(Duration::from_secs(100)),
                per_app_idle: Some(Duration::from_secs(60)),
                ..Default::default()
            },
            &cfg(),
        );
        assert_eq!(d, RecycleDecision::Recycle);
    }

    #[test]
    fn decide_absent_recycle_enabled_treated_as_recyclable() {
        // recycle_enabled=None(旧 app 无注解)= 免费默认可回收
        let d = decide_recycle(
            &RecycleEvalInput {
                replicas: 1,
                age: Some(Duration::from_secs(1000)),
                idle: Some(Duration::from_secs(500_000)),
                ..Default::default()
            },
            &cfg(),
        );
        assert_eq!(d, RecycleDecision::Recycle);
    }

    #[test]
    fn decide_explicit_recyclable_true_recycles() {
        let d = decide_recycle(
            &RecycleEvalInput {
                replicas: 1,
                recycle_enabled: Some(true),
                age: Some(Duration::from_secs(1000)),
                idle: Some(Duration::from_secs(500_000)),
                ..Default::default()
            },
            &cfg(),
        );
        assert_eq!(d, RecycleDecision::Recycle);
    }

    #[test]
    fn decide_recycles_sustained_unhealthy_even_never_accessed() {
        // 长期 NotReady（连续两轮）+ 从未被访问（idle=None 永久 grace）→ 回收。
        // 僵尸容器永远等不到 idle 信号，grace 分支会把它变成永久漏回收。
        let d = decide_recycle(
            &RecycleEvalInput {
                replicas: 1,
                ready_replicas: 0,
                unhealthy_for: Some(Duration::from_secs(7200)),
                age: Some(Duration::from_secs(1000)),
                idle: None,
                ..Default::default()
            },
            &cfg(),
        );
        assert_eq!(d, RecycleDecision::Recycle);
    }

    #[test]
    fn decide_unhealthy_single_observation_keeps_idle_rules() {
        // 单轮 NotReady（可能只是启动窗口）不触发健康回收，走原闲置分支。
        let d = decide_recycle(
            &RecycleEvalInput {
                replicas: 1,
                ready_replicas: 0,
                unhealthy_for: None,
                age: Some(Duration::from_secs(1000)),
                idle: None,
                ..Default::default()
            },
            &cfg(),
        );
        assert_eq!(d, RecycleDecision::Skip(SkipReason::NeverAccessed));
    }

    #[test]
    fn decide_unhealthy_respects_opt_out_and_wake() {
        // 健康回收不越过 opt-out 与唤醒中：付费/唤醒优先级高于清理。
        let base = RecycleEvalInput {
            replicas: 1,
            ready_replicas: 0,
            unhealthy_for: Some(Duration::from_secs(7200)),
            age: Some(Duration::from_secs(1000)),
            idle: None,
            ..Default::default()
        };
        assert_eq!(
            decide_recycle(
                &RecycleEvalInput {
                    recycle_enabled: Some(false),
                    ..base.clone()
                },
                &cfg()
            ),
            RecycleDecision::Skip(SkipReason::OptOut)
        );
        assert_eq!(
            decide_recycle(
                &RecycleEvalInput {
                    is_waking: true,
                    ..base
                },
                &cfg()
            ),
            RecycleDecision::Skip(SkipReason::WakeInFlight)
        );
    }

    #[test]
    fn decide_unhealthy_ignores_zero_replicas() {
        // replicas=0 时 sustained 标志可能滞后（集合清理前的过渡轮），
        // NotRunning 优先短路，不因脏集合误判。
        let d = decide_recycle(
            &RecycleEvalInput {
                replicas: 0,
                ready_replicas: 0,
                unhealthy_for: Some(Duration::from_secs(7200)),
                ..Default::default()
            },
            &cfg(),
        );
        assert_eq!(d, RecycleDecision::Skip(SkipReason::NotRunning));
    }

    #[test]
    fn unhealthy_does_not_override_recent_access_or_app_idle_policy() {
        let mut input = RecycleEvalInput {
            replicas: 1,
            ready_replicas: 0,
            unhealthy_for: Some(Duration::from_secs(7200)),
            age: Some(Duration::from_secs(100_000)),
            idle: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        assert_eq!(
            decide_recycle(&input, &cfg()),
            RecycleDecision::Skip(SkipReason::BelowThreshold)
        );
        input.idle = Some(Duration::from_secs(10_000));
        input.per_app_idle = Some(Duration::from_secs(20_000));
        assert_eq!(
            decide_recycle(&input, &cfg()),
            RecycleDecision::Skip(SkipReason::BelowThreshold)
        );
    }

    #[test]
    fn decide_unknown_age_does_not_block_recycle() {
        // age=None(无法判 protection)→ 不因 protection 跳过,继续后续检查;idle 超阈值 → Recycle
        let d = decide_recycle(
            &RecycleEvalInput {
                replicas: 1,
                idle: Some(Duration::from_secs(500_000)),
                ..Default::default() // age 默认 None
            },
            &cfg(),
        );
        assert_eq!(d, RecycleDecision::Recycle);
    }

    // ---- age_of ----

    #[test]
    fn age_of_parses_rfc3339_past() {
        let t = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let age = age_of(&t).expect("past date parses");
        // ~3600s,留容差
        assert!(age >= Duration::from_secs(3500) && age <= Duration::from_secs(3700));
    }

    #[test]
    fn age_of_future_returns_none() {
        // future → signed_duration_since 为负 → to_std 失败 → None
        let t = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        assert!(age_of(&t).is_none());
    }

    #[test]
    fn age_of_invalid_returns_none() {
        assert!(age_of("not-a-date").is_none());
    }

    #[test]
    fn unhealthy_history_resets_on_replacement_and_observation_gaps() {
        let now = tokio::time::Instant::now();
        let wall = Utc::now();
        let identity = ObservationIdentity {
            lifecycle_id: "lifeone".into(),
            created_at: Some("createdone".into()),
        };
        let first = UnhealthyObservation::observe(None, identity.clone(), now, wall);
        let later = now + Duration::from_secs(7200);
        let continued = UnhealthyObservation::observe(Some(&first), identity.clone(), later, wall);
        assert_eq!(continued.since, now);
        for replacement in [
            ObservationIdentity {
                lifecycle_id: "lifetwo".into(),
                ..identity.clone()
            },
            ObservationIdentity {
                created_at: Some("createdtwo".into()),
                ..identity.clone()
            },
        ] {
            assert_eq!(
                UnhealthyObservation::observe(Some(&first), replacement, later, wall).since,
                later
            );
        }
        // Missing/inactive identity, an omitted app in a partial listing, healthy
        // state and a failed scan all omit the previous sample on the next round.
        assert_eq!(
            UnhealthyObservation::observe(None, identity, later, wall).since,
            later
        );
        let input = RecycleEvalInput {
            replicas: 1,
            unhealthy_for: Some(Duration::from_secs(1)),
            ..Default::default()
        };
        assert_eq!(
            decide_recycle(&input, &cfg()),
            RecycleDecision::Skip(SkipReason::NeverAccessed)
        );
    }

    #[cfg(feature = "userapp-turso")]
    #[tokio::test]
    async fn never_accessed_recycle_can_enter_transition_but_does_not_replace_new_access() {
        let dir = tempfile::tempdir().unwrap();
        let store = rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &dir.path().join("activity.db"),
        )
        .await
        .unwrap();
        let identity = store.ensure_identity("neveraccessed").await.unwrap();
        let activity = app_manager::AppActivityRegistry::new(Duration::from_secs(60));
        activity.bind_lifecycle(
            &identity.app_id,
            &identity.lifecycle_id,
            identity.lifecycle_epoch,
        );
        let baseline = Utc::now() - chrono::Duration::hours(3);
        let observed = recycle_access_baseline(&activity, &identity, None, Some(baseline)).unwrap();
        assert_eq!(observed, baseline);
        drop(
            activity
                .try_begin_recycle(&identity.app_id, observed)
                .expect("never-accessed path reaches physical recycling"),
        );
        activity.seed_accessed(&identity.app_id);
        assert!(
            activity
                .try_begin_recycle(&identity.app_id, observed)
                .is_none(),
            "a late access invalidates recycling"
        );
        let current = activity.last_accessed_at(&identity.app_id).unwrap();
        assert_eq!(
            recycle_access_baseline(&activity, &identity, None, Some(baseline)),
            Some(current)
        );
        let mut replacement = identity.clone();
        replacement.lifecycle_id = "replacement".into();
        replacement.lifecycle_epoch += 1;
        activity.bind_lifecycle(
            &replacement.app_id,
            &replacement.lifecycle_id,
            replacement.lifecycle_epoch,
        );
        assert!(recycle_access_baseline(&activity, &identity, None, Some(baseline)).is_none());
        store.shutdown().await.unwrap();
    }

    #[cfg(feature = "userapp-turso")]
    mod wake_recovery {
        use super::*;
        use container_runtime_api::{
            ContainerRuntimeResult, DeploymentStatus, UserAppDeploymentRuntime, WorkspaceRuntime,
        };
        use shared_types::*;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct Runtime {
            store: Arc<dyn UserAppLifecycleStore>,
            releases: AtomicUsize,
            fail_release: AtomicBool,
        }
        #[async_trait::async_trait]
        impl WorkspaceRuntime for Runtime {}
        #[async_trait::async_trait]
        impl UserAppDeploymentRuntime for Runtime {
            async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
                Ok(vec![])
            }
            async fn release_app_operation_receipt(
                &self,
                context: &UserAppExecutionContext,
                receipt: &UserAppOperationLeaseReceipt,
            ) -> ContainerRuntimeResult<()> {
                let record = self
                    .store
                    .get_operation(&context.app_id, &context.operation_id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    record.state,
                    UserAppOperationState::Failed,
                    "terminal commit must precede physical lease release"
                );
                let binding = self
                    .store
                    .get_operation_lease(&context.app_id, &context.operation_id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(&binding.context, context);
                assert_eq!(&binding.receipt, receipt);
                if self.fail_release.load(Ordering::SeqCst) {
                    return Err(container_runtime_api::ContainerRuntimeError::Conflict(
                        "injected release failure".into(),
                    ));
                }
                self.releases.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        struct Fixture {
            _dir: tempfile::TempDir,
            store: Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
            service: app_manager::service::AppService,
            runtime: Arc<Runtime>,
            app: UserAppLifecycleRecord,
            running: UserAppOperationRecord,
        }

        async fn fixture(acknowledged: bool) -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(
                rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                    &dir.path().join("wake.db"),
                )
                .await
                .unwrap(),
            );
            let app = store.ensure_identity("expiredwake").await.unwrap();
            let admitted = store
                .admit(&UserAppAdmission {
                    app_id: app.app_id.clone(),
                    lifecycle_id: Some(app.lifecycle_id.clone()),
                    operation_id: "oldwake".into(),
                    request_id: Some("originalrequest".into()),
                    request_fingerprint: "a".repeat(64),
                    kind: UserAppOperationKind::Start,
                    command: Some(UserAppControlCommand::Start { traffic: true }),
                    metadata: None,
                    runtime_policy_on_success: None,
                })
                .await
                .unwrap();
            let UserAppAdmissionOutcome::Accepted(op) = admitted else {
                panic!("fresh admission")
            };
            let context = UserAppExecutionContext {
                app_id: app.app_id.clone(),
                lifecycle_id: app.lifecycle_id.clone(),
                operation_id: op.operation_id.clone(),
                executor_id: "originalworker".into(),
                request_fingerprint: op.request_fingerprint.clone(),
            };
            let target = UserAppMutationTarget {
                context: context.clone(),
                resource: AppResourceIdentity {
                    kind: AppResourceKind::Deployment,
                    name: "captureddeployment".into(),
                    uid: "capturedworkloaduid".into(),
                    resource_version: Some("1".into()),
                },
            };
            let running = store.advance(&UserAppOperationProgress {
                app_id: app.app_id.clone(), lifecycle_id: app.lifecycle_id.clone(), operation_id: op.operation_id,
                expected_revision: op.revision, executor_id: context.executor_id.clone(), state: UserAppOperationState::Running,
                step: "traffic_wake_observing".into(), checkpoint: serde_json::json!({"target":target,"start_write_acknowledged":acknowledged}),
                error_code: None, error_message: None,
            }).await.unwrap();
            store
                .bind_operation_lease(
                    &context,
                    &UserAppOperationLeaseReceipt::Kubernetes {
                        service_type: ServiceType::Userapp,
                        namespace: "review".into(),
                        name: "wakelease".into(),
                        uid: "leaseuid".into(),
                        resource_version: "1".into(),
                        token: "oldwaketoken".into(),
                    },
                )
                .await
                .unwrap();
            let runtime = Arc::new(Runtime {
                store: store.clone(),
                releases: AtomicUsize::new(0),
                fail_release: AtomicBool::new(false),
            });
            let service = app_manager::service::AppService::new(
                app_manager::AppManagerConfig {
                    access_mode: app_manager::AppAccessMode::Docker,
                    http_expose: container_runtime_api::HttpExpose::Pingora,
                    ..Default::default()
                },
                runtime.clone(),
                Arc::new(app_manager::AppActivityRegistry::new(Duration::from_secs(
                    60,
                ))),
                None,
                store.clone(),
            )
            .await
            .unwrap();
            let app = store.get_application(&app.app_id).await.unwrap().unwrap();
            Fixture {
                _dir: dir,
                store,
                service,
                runtime,
                app,
                running,
            }
        }

        #[tokio::test]
        async fn stale_acknowledged_wake_recovers_once_and_allows_new_control() {
            let f = fixture(true).await;
            let now = f.running.created_at + chrono::Duration::hours(3);
            let recover = || {
                recover_expired_wake_observation(
                    f.store.as_ref(),
                    &f.service,
                    &f.app,
                    now,
                    Duration::from_secs(60),
                )
            };
            let (a, b) = tokio::join!(recover(), recover());
            assert_eq!(
                usize::from(matches!(a, Ok(true))) + usize::from(matches!(b, Ok(true))),
                1,
                "{a:?} / {b:?}"
            );
            assert_eq!(f.runtime.releases.load(Ordering::SeqCst), 1);
            let terminal = f
                .store
                .get_operation(&f.app.app_id, &f.running.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(terminal.state, UserAppOperationState::Failed);
            assert_eq!(terminal.step, "traffic_wake_observation_failed");
            assert_eq!(terminal.checkpoint, f.running.checkpoint);
            assert!(
                f.store
                    .get_operation_lease(&f.app.app_id, &f.running.operation_id)
                    .await
                    .unwrap()
                    .is_none()
            );
            let new_stop = f
                .store
                .admit(&UserAppAdmission {
                    app_id: f.app.app_id.clone(),
                    lifecycle_id: Some(f.app.lifecycle_id.clone()),
                    operation_id: "nextstop".into(),
                    request_id: Some("nextstoprequest".into()),
                    request_fingerprint: "b".repeat(64),
                    kind: UserAppOperationKind::Stop,
                    command: Some(UserAppControlCommand::Stop {
                        wake_on_traffic: true,
                    }),
                    metadata: None,
                    runtime_policy_on_success: None,
                })
                .await
                .unwrap();
            assert!(matches!(new_stop, UserAppAdmissionOutcome::Accepted(_)));
            assert!(
                f.store
                    .advance(&UserAppOperationProgress {
                        app_id: f.app.app_id.clone(),
                        lifecycle_id: f.app.lifecycle_id.clone(),
                        operation_id: f.running.operation_id.clone(),
                        expected_revision: f.running.revision,
                        executor_id: f.running.executor_id.clone().unwrap(),
                        state: UserAppOperationState::Succeeded,
                        step: "late_completion".into(),
                        checkpoint: f.running.checkpoint.clone(),
                        error_code: None,
                        error_message: None,
                    })
                    .await
                    .is_err()
            );
            assert_eq!(
                f.store
                    .get_application(&f.app.app_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .active_operations
                    .prod
                    .as_deref(),
                Some("nextstop")
            );
            f.store.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn wake_recovery_respects_budget_evidence_and_failed_lease_release() {
            let f = fixture(true).await;
            let now = f.running.created_at + chrono::Duration::hours(3);
            for (time, budget) in [(f.running.created_at, 60), (now, 4 * 3600)] {
                assert!(
                    !recover_expired_wake_observation(
                        f.store.as_ref(),
                        &f.service,
                        &f.app,
                        time,
                        Duration::from_secs(budget)
                    )
                    .await
                    .unwrap()
                );
            }
            assert_eq!(
                f.store
                    .get_operation(&f.app.app_id, "oldwake")
                    .await
                    .unwrap()
                    .unwrap(),
                f.running
            );
            f.runtime.fail_release.store(true, Ordering::SeqCst);
            assert!(
                recover_expired_wake_observation(
                    f.store.as_ref(),
                    &f.service,
                    &f.app,
                    now,
                    Duration::from_secs(60)
                )
                .await
                .is_err()
            );
            let terminal = f
                .store
                .get_operation(&f.app.app_id, "oldwake")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(terminal.state, UserAppOperationState::Failed);
            assert!(
                f.store
                    .get_operation_lease(&f.app.app_id, "oldwake")
                    .await
                    .unwrap()
                    .is_some()
            );
            f.runtime.fail_release.store(false, Ordering::SeqCst);
            f.service
                .retry_control_operation(
                    &f.app.app_id,
                    "oldwake",
                    UserAppRetryRequest {
                        lifecycle_id: f.app.lifecycle_id.clone(),
                        expected_revision: terminal.revision,
                    },
                )
                .await
                .unwrap();
            assert!(
                f.store
                    .get_operation_lease(&f.app.app_id, "oldwake")
                    .await
                    .unwrap()
                    .is_none()
            );
            f.store.shutdown().await.unwrap();

            let f = fixture(false).await;
            let now = f.running.created_at + chrono::Duration::hours(24);
            assert!(
                !recover_expired_wake_observation(
                    f.store.as_ref(),
                    &f.service,
                    &f.app,
                    now,
                    Duration::from_secs(60)
                )
                .await
                .unwrap()
            );
            assert_eq!(
                f.store
                    .get_operation(&f.app.app_id, "oldwake")
                    .await
                    .unwrap()
                    .unwrap(),
                f.running
            );
            assert_eq!(f.runtime.releases.load(Ordering::SeqCst), 0);
            f.store.shutdown().await.unwrap();
        }
    }
}
