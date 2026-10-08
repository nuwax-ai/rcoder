//! UserApp 代理失败诊断顾问（engine 实现：就绪 reader 的失败路径浓缩视图）。
//!
//! 失败出口不能每个请求都打 app-cli 管理面——这里做 30s TTL 的 per-app
//! 短缓存 + 单飞行合并：错误风暴只穿透一次观察。reader 核验观察时的
//! 实例身份；缓存提示最多延迟 30s，不作为当前运行态或控制操作的依据。
//! 缓存只在失败路径填充，无故障流量零开销。

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rcoder_proxy::error_page::{UserAppProxyFailureAdvisor, UserAppProxyFailureHint};
use shared_types::{UserAppReadinessObservation, UserappStage};

use crate::app_state::AppState;

/// 提示缓存 TTL（错误风暴下的观察穿透上限；非 SLA）。
const HINT_TTL: Duration = Duration::from_secs(30);
/// 单次观察预算上限（失败路径诊断绝不拖长代理等待）。
const ADVISE_BUDGET_CAP: Duration = Duration::from_secs(2);
const HINT_CACHE_CAPACITY: usize = 512;

type HintKey = (String, &'static str);
type Inflight = dashmap::DashMap<HintKey, Arc<tokio::sync::Mutex<()>>>;

/// Declared before the local gate Arc so its Drop runs after that Arc is released,
/// including cancellation and cache-hit exits. Entry acquisition and removal use
/// the same DashMap shard lock; a map-only Arc has no active observer or waiter.
struct ObservationFlight<'a> {
    inflight: &'a Inflight,
    key: HintKey,
}

impl Drop for ObservationFlight<'_> {
    fn drop(&mut self) {
        self.inflight
            .remove_if(&self.key, |_, gate| Arc::strong_count(gate) == 1);
    }
}

struct CachedHint {
    at: Instant,
    hint: Option<UserAppProxyFailureHint>,
}

pub struct UserAppProxyFailureAdvisorImpl {
    state: std::sync::Weak<AppState>,
    cache: tokio::sync::Mutex<HashMap<HintKey, CachedHint>>,
    /// per-key 单飞行闸（错误风暴下不同 app 的观察互不排队）。
    inflight: Inflight,
}

impl UserAppProxyFailureAdvisorImpl {
    pub(crate) fn new(state: std::sync::Weak<AppState>) -> Self {
        Self {
            state,
            cache: tokio::sync::Mutex::new(HashMap::new()),
            inflight: dashmap::DashMap::new(),
        }
    }
}

#[async_trait::async_trait]
impl UserAppProxyFailureAdvisor for UserAppProxyFailureAdvisorImpl {
    async fn advise(
        &self,
        app_id: &str,
        stage: &str,
        budget: Duration,
    ) -> Option<UserAppProxyFailureHint> {
        let stage = UserappStage::parse(stage)?;
        let budget = budget.min(ADVISE_BUDGET_CAP);
        let key = (app_id.to_string(), stage.as_str());
        // Waiting for another observer is part of the same caller budget.
        tokio::time::timeout(
            budget,
            self.cached_observation(key, self.observe(app_id, stage, budget)),
        )
        .await
        .unwrap_or(None)
    }
}

impl UserAppProxyFailureAdvisorImpl {
    /// 外层 Option 表示缓存命中；Some(None) 缓存“没有诊断”，避免重复观察。
    async fn cached_hint(&self, key: &HintKey) -> Option<Option<UserAppProxyFailureHint>> {
        self.cache
            .lock()
            .await
            .get(key)
            .filter(|cached| cached.at.elapsed() < HINT_TTL)
            .map(|cached| cached.hint.clone())
    }

    async fn cached_observation(
        &self,
        key: HintKey,
        observe: impl Future<Output = Option<UserAppProxyFailureHint>>,
    ) -> Option<UserAppProxyFailureHint> {
        if let Some(hint) = self.cached_hint(&key).await {
            return hint;
        }
        let _flight = ObservationFlight {
            inflight: &self.inflight,
            key: key.clone(),
        };
        let gate = self
            .inflight
            .entry(key.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = gate.lock().await;
        if let Some(hint) = self.cached_hint(&key).await {
            return hint;
        }
        // The cache mutex only protects map access. Other apps may observe
        // while this app waits for its runtime, and cancellation releases flight.
        let hint = observe.await;
        let mut cache = self.cache.lock().await;
        cache.insert(
            key,
            CachedHint {
                at: Instant::now(),
                hint: hint.clone(),
            },
        );
        // 超出容量后清空短时诊断缓存，后续请求重新观察。
        if cache.len() > HINT_CACHE_CAPACITY {
            cache.clear();
        }
        hint
    }

    async fn observe(
        &self,
        app_id: &str,
        stage: UserappStage,
        budget: Duration,
    ) -> Option<UserAppProxyFailureHint> {
        let state = self.state.upgrade()?;
        let reader = state.app_service.readiness_reader()?;
        match reader.observe(app_id, stage, budget).await {
            Ok(UserAppReadinessObservation::Snapshot { snapshot, .. }) => {
                Some(UserAppProxyFailureHint {
                    readiness_status: Some(snapshot.status),
                    error_origin_confirmed: snapshot.proxy.error_origin_contract.as_deref()
                        == Some(shared_types::PINGAP_ETYPE_ORIGIN_CONTRACT),
                })
            }
            // 无快照（不可达/换代/不支持）= 无来源证据：不确认替换。停止态
            // 与计算资源缺失（未部署/已删除/被回收）本身可作为文案证据返回。
            // Missing 仅 prod 透传：dev scope 无计算资源 ≠ 应用不存在（dev
            // 运行时未启动/被回收是常态），dev 路由的 cause 细分按方案延后。
            Ok(UserAppReadinessObservation::NoCompute { state }) => {
                // 全枚举显式匹配：NoComputeState 新增/改名时编译期即暴露此分支
                let status = match state {
                    shared_types::UserAppNoComputeState::Stopped => {
                        Some(shared_types::UserAppReadinessStatus::Stopped)
                    }
                    shared_types::UserAppNoComputeState::Missing if stage == UserappStage::Prod => {
                        Some(shared_types::UserAppReadinessStatus::NotDeployed)
                    }
                    shared_types::UserAppNoComputeState::Missing
                    | shared_types::UserAppNoComputeState::Starting
                    | shared_types::UserAppNoComputeState::Stopping
                    | shared_types::UserAppNoComputeState::Failed
                    | shared_types::UserAppNoComputeState::Unknown => None,
                };
                status.map(|readiness_status| UserAppProxyFailureHint {
                    readiness_status: Some(readiness_status),
                    error_origin_confirmed: false,
                })
            }
            Ok(_) | Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Weak;

    #[tokio::test]
    async fn failure_advisor_invalid_stage_cannot_reuse_dev_hint() {
        let advisor = UserAppProxyFailureAdvisorImpl::new(Weak::new());
        let hint = UserAppProxyFailureHint {
            readiness_status: Some(shared_types::UserAppReadinessStatus::Failed),
            error_origin_confirmed: true,
        };
        advisor
            .cached_observation(("app123".into(), "dev"), async { Some(hint.clone()) })
            .await;

        for stage in ["", "staging", "DEV", "prod"] {
            assert!(
                advisor
                    .advise("app123", stage, Duration::from_secs(1))
                    .await
                    .is_none(),
                "stage {stage:?} must not reuse the dev hint"
            );
        }
        assert_eq!(
            advisor
                .advise("app123", "dev", Duration::from_secs(1))
                .await,
            Some(hint)
        );
        assert!(advisor.inflight.is_empty());
    }

    #[tokio::test]
    async fn failure_advisor_coalesces_waiters_without_blocking_other_apps() {
        let advisor = UserAppProxyFailureAdvisorImpl::new(Weak::new());
        let key = ("slowapp".into(), "dev");
        let release = tokio::sync::Notify::new();
        let mut first = Box::pin(advisor.cached_observation(key.clone(), async {
            release.notified().await;
            None
        }));
        assert!(futures::poll!(first.as_mut()).is_pending());
        let mut waiter = Box::pin(advisor.cached_observation(key, async {
            panic!("a waiter must reuse the cached observation, including None")
        }));
        assert!(futures::poll!(waiter.as_mut()).is_pending());

        let other = tokio::time::timeout(
            Duration::from_secs(1),
            advisor.cached_observation(("fastapp".into(), "prod"), async { None }),
        )
        .await;
        assert!(
            other.is_ok(),
            "a slow app must not hold the shared cache lock"
        );
        assert_eq!(advisor.inflight.len(), 1);

        release.notify_one();
        assert!(first.await.is_none());
        assert!(waiter.await.is_none());
        assert!(
            advisor.inflight.is_empty(),
            "cache-hit waiters must release the flight"
        );
    }

    #[tokio::test]
    async fn failure_advisor_cancellation_and_caller_budget_release_flights() {
        let advisor = UserAppProxyFailureAdvisorImpl::new(Weak::new());
        let key = ("cancelapp".into(), "dev");
        let mut first = Box::pin(advisor.cached_observation(key, std::future::pending()));
        assert!(futures::poll!(first.as_mut()).is_pending());

        // Exercise the public caller budget while another request owns the gate.
        assert!(
            advisor
                .advise("cancelapp", "dev", Duration::from_millis(10))
                .await
                .is_none()
        );
        assert_eq!(advisor.inflight.len(), 1);
        drop(first);
        assert!(advisor.inflight.is_empty());

        assert!(
            advisor
                .advise("cancelapp", "dev", Duration::from_secs(1))
                .await
                .is_none()
        );
        assert!(
            advisor.inflight.is_empty(),
            "a cancelled observer must not strand the key"
        );
    }
}
