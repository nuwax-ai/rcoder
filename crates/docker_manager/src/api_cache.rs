//! Docker API 缓存（Moka），减少 Docker API 调用次数

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;
use tracing::info;

use crate::ContainerQueryResultArc;

/// Bounded query identity; replacing the Arc invalidates earlier cache fills.
pub(crate) struct CacheGeneration(Arc<()>);

/// Docker API 缓存
///
/// 使用 Moka 缓存库实现高性能缓存，减少 Docker API 调用次数
/// 使用结构体包装，提高代码可读性和减少 clone 开销
pub struct DockerApiCache {
    // Only local cache commits run under this lock; all Docker I/O stays outside.
    generation: tokio::sync::Mutex<Arc<()>>,
    /// 容器状态缓存 (identifier -> `Option<ContainerQueryResultArc>`)
    /// 支持 None 值缓存，用于缓存 404 响应
    status_cache: Cache<String, Option<ContainerQueryResultArc>>,

    /// 网络信息缓存 (container_id -> Option<Arc<HashMap<network_name, ip_address>>>)
    /// 支持 None 值缓存
    network_cache: Cache<String, Option<Arc<HashMap<String, String>>>>,
}

impl DockerApiCache {
    /// 创建新的缓存实例
    ///
    /// # 参数
    /// * `status_ttl` - 状态缓存 TTL（秒）
    /// * `network_ttl` - 网络缓存 TTL（秒）
    /// * `max_capacity` - 缓存最大容量
    pub fn new(status_ttl: u64, network_ttl: u64, max_capacity: u64) -> Self {
        info!(
            "Initializing Docker API cache: status_ttl={}s, network_ttl={}s, max_capacity={}",
            status_ttl, network_ttl, max_capacity
        );

        Self {
            generation: tokio::sync::Mutex::new(Arc::new(())),
            status_cache: Cache::builder()
                .support_invalidation_closures()
                .max_capacity(max_capacity)
                .time_to_live(Duration::from_secs(status_ttl))
                .build(),
            network_cache: Cache::builder()
                .max_capacity(max_capacity)
                .time_to_live(Duration::from_secs(network_ttl))
                .build(),
        }
    }

    /// 获取状态缓存
    pub async fn get_status(&self, identifier: &str) -> Option<Option<ContainerQueryResultArc>> {
        self.status_cache.get(identifier).await
    }

    /// Snapshot before I/O; invalidation rotates this bounded identity token.
    pub(crate) async fn begin_query(&self) -> CacheGeneration {
        CacheGeneration(self.generation.lock().await.clone())
    }

    pub(crate) async fn publish_status(
        &self,
        query: &CacheGeneration,
        identifiers: &[String],
        value: Option<ContainerQueryResultArc>,
    ) -> bool {
        let current = self.generation.lock().await;
        if !Arc::ptr_eq(&current, &query.0) {
            return false;
        }
        for identifier in identifiers {
            self.status_cache
                .insert(identifier.clone(), value.clone())
                .await;
        }
        true
    }

    #[cfg(test)]
    pub async fn insert_status(&self, identifier: String, value: Option<ContainerQueryResultArc>) {
        let query = self.begin_query().await;
        assert!(self.publish_status(&query, &[identifier], value).await);
    }

    /// 获取网络缓存
    pub async fn get_network(
        &self,
        container_id: &str,
    ) -> Option<Option<Arc<HashMap<String, String>>>> {
        self.network_cache.get(container_id).await
    }

    pub(crate) async fn publish_network(
        &self,
        query: &CacheGeneration,
        container_id: String,
        value: Option<Arc<HashMap<String, String>>>,
    ) -> bool {
        let current = self.generation.lock().await;
        if !Arc::ptr_eq(&current, &query.0) {
            return false;
        }
        self.network_cache.insert(container_id, value).await;
        true
    }

    #[cfg(test)]
    pub async fn insert_network(
        &self,
        container_id: String,
        value: Option<Arc<HashMap<String, String>>>,
    ) {
        let query = self.begin_query().await;
        assert!(self.publish_network(&query, container_id, value).await);
    }

    /// Retire only aliases that still reference the removed physical container.
    pub async fn invalidate_container(&self, container_id: &str) -> crate::DockerResult<()> {
        let mut current = self.generation.lock().await;
        *current = Arc::new(());
        let retired_id = container_id.to_owned();
        self.status_cache
            .invalidate_entries_if(move |_, value| {
                value
                    .as_ref()
                    .is_some_and(|info| info.container_id == retired_id)
            })
            .map_err(|error| {
                crate::DockerError::ConfigurationError(format!(
                    "invalidate container cache: {error}"
                ))
            })?;
        self.network_cache.invalidate(container_id).await;
        Ok(())
    }

    /// 使缓存失效
    pub async fn invalidate(&self, identifier: &str) {
        self.invalidate_all(&[identifier.to_owned()]).await;
    }

    /// Ignore a delayed 404 after a lifecycle change instead of evicting new state.
    pub(crate) async fn invalidate_if_current(
        &self,
        query: &CacheGeneration,
        identifier: &str,
    ) -> bool {
        let mut current = self.generation.lock().await;
        if !Arc::ptr_eq(&current, &query.0) {
            return false;
        }
        *current = Arc::new(());
        self.status_cache.invalidate(identifier).await;
        self.network_cache.invalidate(identifier).await;
        true
    }

    /// 使所有相关缓存失效（用于容器生命周期变化后）
    pub async fn invalidate_all(&self, identifiers: &[String]) {
        let mut current = self.generation.lock().await;
        *current = Arc::new(());
        for id in identifiers {
            self.status_cache.invalidate(id.as_str()).await;
            self.network_cache.invalidate(id.as_str()).await;
        }
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;

    #[tokio::test]
    async fn all_invalidation_entrypoints_reject_old_fills_and_allow_current_fills() {
        for method in 0..3 {
            let cache = DockerApiCache::new(600, 600, 100);
            let before = cache.begin_query().await;
            match method {
                0 => cache.invalidate_all(&["unrelated".into()]).await,
                1 => cache.invalidate("unrelated").await,
                _ => cache.invalidate_container("unrelated").await.unwrap(),
            }
            assert!(
                !cache
                    .publish_status(&before, &["old-id".into(), "old-name".into()], None)
                    .await
            );
            assert!(!cache.publish_network(&before, "old-id".into(), None).await);
            assert!(cache.get_status("old-id").await.is_none());
            assert!(cache.get_status("old-name").await.is_none());
            assert!(cache.get_network("old-id").await.is_none());
            let current = cache.begin_query().await;
            assert!(
                cache
                    .publish_status(&current, &["new-id".into(), "new-name".into()], None)
                    .await
            );
            assert!(cache.publish_network(&current, "new-id".into(), None).await);
            assert!(matches!(cache.get_status("new-id").await, Some(None)));
            assert!(matches!(cache.get_status("new-name").await, Some(None)));
            assert!(matches!(cache.get_network("new-id").await, Some(None)));
        }
    }

    #[tokio::test]
    async fn delayed_not_found_cannot_invalidate_current_alias_or_rotate_current_generation() {
        let cache = DockerApiCache::new(600, 600, 100);
        let before = cache.begin_query().await;
        cache.invalidate_all(&["builder".into()]).await;
        let current = cache.begin_query().await;
        assert!(
            cache
                .publish_status(&current, &["builder".into()], None)
                .await
        );
        assert!(!cache.invalidate_if_current(&before, "builder").await);
        assert!(matches!(cache.get_status("builder").await, Some(None)));
        assert!(cache.publish_network(&current, "new-id".into(), None).await);
        assert!(cache.invalidate_if_current(&current, "builder").await);
        assert!(cache.get_status("builder").await.is_none());
        assert!(!cache.publish_network(&current, "new-id".into(), None).await);
    }
}
