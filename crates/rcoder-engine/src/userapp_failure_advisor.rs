//! UserApp 代理失败诊断顾问（engine 实现：就绪 reader 的失败路径浓缩视图）。
//!
//! 失败出口不能每个请求都打 app-cli 管理面——这里做 30s TTL 的 per-app
//! 短缓存 + 单飞行合并：错误风暴只穿透一次观察；缓存值附带观察时间，
//! Stop/换代由 reader 的换代检测保证不沿用陈旧 ready（观察本身带
//! observation_revision 复核）。缓存只在失败路径填充，无故障流量零开销。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rcoder_proxy::error_page::{UserAppProxyFailureAdvisor, UserAppProxyFailureHint};
use shared_types::UserAppReadinessObservation;

use crate::app_state::AppState;

/// 提示缓存 TTL（错误风暴下的观察穿透上限；非 SLA）。
const HINT_TTL: Duration = Duration::from_secs(30);
/// 单次观察预算上限（失败路径诊断绝不拖长代理等待）。
const ADVISE_BUDGET_CAP: Duration = Duration::from_secs(2);

struct CachedHint {
    at: Instant,
    hint: Arc<Option<UserAppProxyFailureHint>>,
}

pub struct UserAppProxyFailureAdvisorImpl {
    state: std::sync::Weak<AppState>,
    cache: tokio::sync::Mutex<HashMap<(String, &'static str), CachedHint>>,
}

impl UserAppProxyFailureAdvisorImpl {
    pub(crate) fn new(state: std::sync::Weak<AppState>) -> Self {
        Self {
            state,
            cache: tokio::sync::Mutex::new(HashMap::new()),
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
        let key = (app_id.to_string(), stage_leak(stage));
        {
            let cache = self.cache.lock().await;
            if let Some(cached) = cache.get(&key)
                && cached.at.elapsed() < HINT_TTL
            {
                return (*cached.hint).clone();
            }
        }
        // 单飞行合并：并发失败只穿透一次观察（锁内做观察——观察自身 ≤2s、
        // 只读，无业务锁；晚到的并发调用在锁后命中新缓存）。
        let mut cache = self.cache.lock().await;
        if let Some(cached) = cache.get(&key)
            && cached.at.elapsed() < HINT_TTL
        {
            return (*cached.hint).clone();
        }
        let hint = Arc::new(
            self.observe(app_id, stage, budget.min(ADVISE_BUDGET_CAP))
                .await,
        );
        cache.insert(
            key,
            CachedHint {
                at: Instant::now(),
                hint: hint.clone(),
            },
        );
        // 缓存有界：失败路径 app 集合有限，仍按容量防御（超出丢最旧——
        // 无序 HashMap 直接 clear 极端场景，正常规模无感）。
        if cache.len() > 512 {
            cache.clear();
        }
        (*hint).clone()
    }
}

impl UserAppProxyFailureAdvisorImpl {
    async fn observe(
        &self,
        app_id: &str,
        stage: &str,
        budget: Duration,
    ) -> Option<UserAppProxyFailureHint> {
        let state = self.state.upgrade()?;
        let stage = shared_types::UserappStage::parse(stage)?;
        let reader = state.app_service.readiness_reader()?;
        match reader.observe(app_id, stage, budget).await {
            Ok(UserAppReadinessObservation::Snapshot { snapshot, .. }) => {
                Some(UserAppProxyFailureHint {
                    readiness_status: Some(snapshot.status),
                    error_origin_confirmed: snapshot.proxy.error_origin_contract.as_deref()
                        == Some(shared_types::PINGAP_ETYPE_ORIGIN_CONTRACT),
                })
            }
            // 无快照（停止/不可达/换代/不支持）= 无来源证据：不确认替换，
            // 停止态本身可作为文案证据返回。
            Ok(UserAppReadinessObservation::NoCompute { detail }) => {
                let stopped = matches!(
                    detail.as_deref(),
                    Some("scaled-to-zero") | Some("container-stopped") | Some("stopped")
                );
                stopped.then_some(UserAppProxyFailureHint {
                    readiness_status: Some(shared_types::UserAppReadinessStatus::Stopped),
                    error_origin_confirmed: false,
                })
            }
            Ok(_) | Err(_) => None,
        }
    }
}

/// 失败路径的 stage 词表收口（代理侧传入 `prod`/`dev` 字面量）。
fn stage_leak(stage: &str) -> &'static str {
    match stage {
        "prod" => "prod",
        _ => "dev",
    }
}
