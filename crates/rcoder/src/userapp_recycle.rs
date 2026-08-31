//! Userapp 闲置自动回收扫描器（后台定时任务）。
//!
//! 周期枚举所有 Running Userapp,比对 `last_accessed_at` 与阈值,闲置超阈值 → `stop_app`(scale0,
//! 不删 PVC/Service/路由)。付费 app(`recycle_enabled=false` 注解)opt-out 跳过;进行中的唤醒跳过;
//! 龄期 < protection 跳过;从未访问(None)跳过 grace。
//!
//! 闲置信号来自 pingora 热路径 `AppAccessTracker::touch`(经 [`AppActivityRegistry`] 维护)。
//! 回收 = scale-to-zero,数据零风险;唤醒由 pingora `request_filter` 的 wake-on-traffic 负责。
//!
//! 回收判定逻辑抽成纯函数 [`decide_recycle`](见模块底),不依赖 AppState/K8s,便于单测覆盖所有分支。

use std::sync::Arc;
use std::time::Duration;

use chrono::DateTime;
use tracing::{debug, info, warn};

use crate::router::AppState;

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
}

impl UserAppRecycleScanner {
    pub(crate) fn new(config: UserAppRecycleRuntimeConfig, state: Arc<AppState>) -> Self {
        Self { config, state }
    }

    pub async fn run(self, mut shutdown_rx: tokio::sync::broadcast::Receiver<()>) {
        info!(
            "[USERAPP_RECYCLE] scanner started (interval={:?}, idle_timeout={:?}, protection={:?})",
            self.config.scan_interval, self.config.idle_timeout, self.config.protection
        );
        let mut interval = tokio::time::interval(self.config.scan_interval);
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
    async fn do_scan(&self) -> anyhow::Result<usize> {
        let apps = self
            .state
            .app_service
            .list_all_app_runtimes()
            .await
            .map_err(|e| anyhow::anyhow!("list_all_app_runtimes: {e}"))?;
        // 闲置时长按 wall-clock 计算（last_accessed 为 DateTime，可跨重启持久化）；
        // 负值（时钟回拨）按 0 处理
        let now = chrono::Utc::now();
        let mut recycled = 0usize;

        for app in apps {
            let age = app.created_at.as_deref().and_then(age_of);
            let last_accessed = self.state.activity.last_accessed_at(&app.app_id);
            let idle =
                last_accessed.map(|t| now.signed_duration_since(t).to_std().unwrap_or_default());
            let decision = decide_recycle(
                &RecycleEvalInput {
                    replicas: app.replicas,
                    recycle_enabled: app.recycle_enabled,
                    is_waking: self.state.activity.is_waking(&app.app_id),
                    age,
                    idle,
                    per_app_idle: app.idle_timeout_seconds.map(Duration::from_secs),
                },
                &self.config,
            );
            let app_id = &app.app_id;
            match decision {
                RecycleDecision::Recycle => {
                    let Some(observed_access) = last_accessed else {
                        continue;
                    };
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
        Ok(recycled)
    }
}

/// RFC3339 创建时间 → 至今的龄期(future/解析失败 → None)
fn age_of(created_at: &str) -> Option<Duration> {
    let t = DateTime::parse_from_rfc3339(created_at).ok()?;
    let diff = chrono::Utc::now().signed_duration_since(t);
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
#[derive(Default)]
struct RecycleEvalInput {
    replicas: i32,
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
    let idle = match input.idle {
        Some(d) => d,
        None => return RecycleDecision::Skip(SkipReason::NeverAccessed),
    };
    let threshold = input.per_app_idle.unwrap_or(cfg.idle_timeout);
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
            idle_timeout: Duration::from_secs(432_000), // 5d
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
                age: Some(Duration::from_secs(1000)),
                idle: Some(Duration::from_secs(500_000)),
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
        // idle=100s < 全局阈值 432000s
        let d = decide_recycle(
            &RecycleEvalInput {
                replicas: 1,
                age: Some(Duration::from_secs(1000)),
                idle: Some(Duration::from_secs(100)),
                ..Default::default()
            },
            &cfg(),
        );
        assert_eq!(d, RecycleDecision::Skip(SkipReason::BelowThreshold));
    }

    #[test]
    fn decide_per_app_threshold_overrides_global() {
        // per-app 阈值=60s;idle=100s > 60 → Recycle(即便全局 432000 本会跳过)
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
        let t = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let age = age_of(&t).expect("past date parses");
        // ~3600s,留容差
        assert!(age >= Duration::from_secs(3500) && age <= Duration::from_secs(3700));
    }

    #[test]
    fn age_of_future_returns_none() {
        // future → signed_duration_since 为负 → to_std 失败 → None
        let t = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        assert!(age_of(&t).is_none());
    }

    #[test]
    fn age_of_invalid_returns_none() {
        assert!(age_of("not-a-date").is_none());
    }
}
