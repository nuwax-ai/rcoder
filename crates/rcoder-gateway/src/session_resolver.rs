//! Session 解析：session_id → identifier
//!
//! 查询 rcoder-control 的 /internal/session/{session_id}/resolve 接口，
//! 并使用 Moka TTL 缓存避免重复查询。

use std::time::Duration;

use moka::future::Cache;
use tracing::debug;

use crate::control_plane_client::ControlPlaneClient;

/// Session 解析结果
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub identifier: String,
    pub service_type: String,
}

/// Session 解析器
pub struct SessionResolver {
    cache: Cache<String, SessionInfo>,
    control_client: ControlPlaneClient,
}

impl SessionResolver {
    pub fn new(control_client: ControlPlaneClient, ttl: Duration) -> Self {
        let cache = Cache::builder()
            .time_to_idle(ttl)
            .max_capacity(10_000)
            .build();
        Self {
            cache,
            control_client,
        }
    }

    /// 解析 session_id → (identifier, service_type)
    pub async fn resolve(&self, session_id: &str) -> anyhow::Result<SessionInfo> {
        if let Some(info) = self.cache.get(session_id).await {
            debug!("[SESSION] cache hit: {} → {}", session_id, info.identifier);
            return Ok(info);
        }

        debug!("[SESSION] cache miss: {}", session_id);
        let resp = self.control_client.resolve_session(session_id).await?;

        if !resp.success {
            let msg = resp.message.unwrap_or_else(|| "unknown error".to_string());
            anyhow::bail!("session resolve failed for {}: {}", session_id, msg);
        }

        let data = resp.data.ok_or_else(|| {
            anyhow::anyhow!("session resolve returned no data for {}", session_id)
        })?;

        let info = SessionInfo {
            identifier: data.identifier.clone(),
            service_type: data.service_type.clone(),
        };

        self.cache
            .insert(session_id.to_string(), info.clone())
            .await;
        debug!(
            "[SESSION] resolved: {} → {} ({})",
            session_id, info.identifier, info.service_type
        );
        Ok(info)
    }
}
