//! Docker API 缓存（Moka），减少 Docker API 调用次数

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;
use tracing::info;

use crate::ContainerQueryResultArc;

/// Docker API 缓存
///
/// 使用 Moka 缓存库实现高性能缓存，减少 Docker API 调用次数
/// 使用结构体包装，提高代码可读性和减少 clone 开销
pub struct DockerApiCache {
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
            status_cache: Cache::builder()
                .max_capacity(max_capacity)
                .time_to_live(Duration::from_secs(status_ttl))
                .build(),
            network_cache: Cache::builder()
                .max_capacity(max_capacity)
                .time_to_live(Duration::from_secs(network_ttl))
                .build(),
        }
    }

    /// 使用默认配置创建缓存实例
    #[allow(dead_code)]
    pub fn with_defaults() -> Self {
        Self::new(10, 15, 10000)
    }

    /// 获取状态缓存
    pub async fn get_status(&self, identifier: &str) -> Option<Option<ContainerQueryResultArc>> {
        self.status_cache.get(identifier).await
    }

    /// 写入状态缓存（支持 None 值）
    pub async fn insert_status(&self, identifier: String, value: Option<ContainerQueryResultArc>) {
        self.status_cache.insert(identifier, value).await;
    }

    /// 获取网络缓存
    pub async fn get_network(
        &self,
        container_id: &str,
    ) -> Option<Option<Arc<HashMap<String, String>>>> {
        self.network_cache.get(container_id).await
    }

    /// 写入网络缓存（支持 None 值）
    pub async fn insert_network(
        &self,
        container_id: String,
        value: Option<Arc<HashMap<String, String>>>,
    ) {
        self.network_cache.insert(container_id, value).await;
    }

    /// 使缓存失效
    pub async fn invalidate(&self, identifier: &str) {
        self.status_cache.invalidate(identifier).await;
        self.network_cache.invalidate(identifier).await;
    }

    /// 使所有相关缓存失效（用于容器生命周期变化后）
    pub async fn invalidate_all(&self, identifiers: &[String]) {
        for id in identifiers {
            self.status_cache.invalidate(id.as_str()).await;
            self.network_cache.invalidate(id.as_str()).await;
        }
    }
}
