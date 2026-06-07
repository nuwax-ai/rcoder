//! identifier → backend_cluster 名的 TTL 缓存
//!
//! 首次请求时通过 rcoder-control 的 pod/ensure 获取。
//! 后续请求直接命中缓存。缓存 TTL 默认 10 分钟（匹配 pod TTL）。

use std::time::Duration;

use moka::future::Cache;
use tracing::{debug, info};

use crate::control_plane_client::{ControlPlaneClient, build_backend_cluster_name};

/// identifier → backend_cluster 名缓存
pub struct ClusterCache {
    cache: Cache<String, String>,
    control_client: ControlPlaneClient,
}

impl ClusterCache {
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

    /// 获取或创建 backend cluster 名
    ///
    /// 缓存命中 → 直接返回
    /// 缓存未命中 → 调用 rcoder-control ensure 接口 → 缓存结果
    pub async fn get_or_ensure(
        &self,
        identifier: &str,
        service_type: &str,
    ) -> anyhow::Result<String> {
        if let Some(cluster) = self.cache.get(identifier).await {
            debug!("[CACHE] hit: {} → {}", identifier, cluster);
            return Ok(cluster);
        }

        debug!(
            "[CACHE] miss: {}, calling rcoder-control ensure",
            identifier
        );
        let resp = self.control_client.ensure_pod(identifier, service_type).await?;

        if !resp.success {
            let msg = resp.message.unwrap_or_else(|| "unknown error".to_string());
            // RCoder 返回 not_found 时不应创建，回退到控制面
            if msg.contains("not_found") {
                debug!("[CACHE] pod not found for {} (RCoder?), will route through control plane", identifier);
                anyhow::bail!("not_found: {}", msg);
            }
            anyhow::bail!("pod/ensure failed for {}: {}", identifier, msg);
        }

        let cluster = build_backend_cluster_name(identifier);
        self.cache.insert(identifier.to_string(), cluster.clone()).await;
        info!(
            "[CACHE] cached: {} → {} (ttl={:.0}s)",
            identifier,
            cluster,
            self.cache.policy().time_to_idle().unwrap_or_default().as_secs_f64()
        );
        Ok(cluster)
    }

    /// 仅查缓存，不触发 pod 创建
    ///
    /// 用于只读路由（GET status、SSE progress）。
    /// 缓存未命中时返回错误，网关会回退到控制面。
    pub async fn get_only(&self, identifier: &str) -> anyhow::Result<String> {
        if let Some(cluster) = self.cache.get(identifier).await {
            debug!("[CACHE] hit (read-only): {} → {}", identifier, cluster);
            return Ok(cluster);
        }
        anyhow::bail!("cache miss (read-only): {}", identifier);
    }

    /// 手动插入缓存（用于 session resolve 后的预缓存）
    pub async fn insert(&self, identifier: &str, cluster: &str) {
        self.cache
            .insert(identifier.to_string(), cluster.to_string())
            .await;
    }

    /// 使缓存条目失效
    pub async fn invalidate(&self, identifier: &str) {
        self.cache.invalidate(identifier).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_backend_cluster_name_consistency() {
        // 确保与 control_plane_client 中的函数一致
        assert_eq!(
            build_backend_cluster_name("user-123"),
            "backend-user-123"
        );
    }
}
