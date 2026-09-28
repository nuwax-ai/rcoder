//! dbx-web（容器内 supervisor 恒起 `DBX_PORT`=4224）只读就绪探测
//! （`/{app_id}/{app_stage}/dbx/readiness`）。
//!
//! 复用 runtime 观察链定位实例（`NotRunning` → `stopped` 白送，dev/prod、
//! Docker/K8s 同构）；拿到实例地址后对 4224 做小预算 HTTP 探测——任意状态码
//! 即 `ready`（GUI 起来就会应答），连接失败即 `starting`。全程只读：
//! 不唤醒、不建 dev 容器、不刷新闲置计时（唤醒由 dbx 代理路径承担）。

use std::sync::{Arc, Weak};
use std::time::Duration;

use container_runtime_api::UserAppRuntimeReadiness;

use crate::app_state::AppState;
use shared_types::{DbxReadinessProber, DbxReadinessStatus, UserappStage};

/// 单次 HTTP 探测预算（定位观察另有 runtime 自身预算；总预算对齐设计 ≤3s）。
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

pub struct DbxReadinessProberImpl {
    state: Weak<AppState>,
    http: reqwest::Client,
}

impl DbxReadinessProberImpl {
    pub(crate) fn new(state: Weak<AppState>) -> Self {
        Self {
            state,
            // 探测专用共享客户端（http_client.rs 约定：消灭裸 Client::new；
            // 无全局总超时，预算由下方 per-request .timeout(PROBE_TIMEOUT) 控制）。
            http: crate::http_client::probe_client().clone(),
        }
    }

    fn state(&self) -> Result<Arc<AppState>, String> {
        self.state
            .upgrade()
            .ok_or_else(|| "app state already dropped".to_string())
    }
}

#[async_trait::async_trait]
impl DbxReadinessProber for DbxReadinessProberImpl {
    async fn probe(&self, app_id: &str, stage: UserappStage) -> Result<DbxReadinessStatus, String> {
        let state = self.state()?;
        let runtime = state.runtime();
        let located = runtime
            .observe_userapp_readiness(app_id, stage)
            .await
            .map_err(|error| format!("Locate {stage:?} instance (app {app_id}): {error}"))?;
        let target = match located {
            UserAppRuntimeReadiness::NotRunning(_) => return Ok(DbxReadinessStatus::Stopped),
            UserAppRuntimeReadiness::Running(target) => target,
        };
        // 定位产物面向 app-cli 管理 API；dbx 探测只取实例 IP，端口换 4224。
        let Some(addr) = target.address.or(target.published_address) else {
            // exec-only 形态（无直连地址）：本版不承载，按启动中呈现由前端重试。
            return Ok(DbxReadinessStatus::Starting);
        };
        let url = format!("http://{}:{}/", addr.ip(), shared_types::DBX_PORT);
        match self.http.get(&url).timeout(PROBE_TIMEOUT).send().await {
            // 任意 HTTP 状态码都算 ready——GUI 起来就会应答，不苛求 200。
            Ok(_) => Ok(DbxReadinessStatus::Ready),
            Err(error) => {
                tracing::debug!("[DBX_READINESS] probe not answering: app {app_id} {url}: {error}");
                Ok(DbxReadinessStatus::Starting)
            }
        }
    }
}
