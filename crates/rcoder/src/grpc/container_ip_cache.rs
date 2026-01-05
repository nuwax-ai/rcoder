//! 容器 IP 缓存模块
//!
//! 提供带 TTL 的容器 IP 缓存，避免频繁调用 Docker API。
//! 缓存使用 `container_name` 作为键（稳定标识符，重启后不变）。
//!
//! ## 设计说明
//!
//! 使用 `moka` 缓存库实现，具有以下优势：
//! - **内置 TTL 支持**：自动过期，无需手动清理
//! - **无锁设计**：使用内部分片实现高并发，无锁污染问题
//! - **高性能**：受 Java Caffeine 缓存启发的高性能实现

use moka::sync::Cache;
use std::time::Duration;
use tracing::{debug, info};

/// 带 TTL 的容器 IP 缓存
///
/// 基于 `moka` 实现，无锁且自动过期
pub struct ContainerIpCache {
    cache: Cache<String, String>,
}

impl ContainerIpCache {
    /// 创建缓存实例
    ///
    /// # 参数
    /// * `ttl_seconds` - 缓存过期时间（秒）
    pub fn new(ttl_seconds: u64) -> Self {
        info!("🗄️ [IP_CACHE] 初始化容器 IP 缓存: TTL={}秒 (使用 moka 无锁缓存)", ttl_seconds);
        
        let cache = Cache::builder()
            // 设置 TTL
            .time_to_live(Duration::from_secs(ttl_seconds))
            // 最大容量（可按需调整）
            .max_capacity(1000)
            .build();
        
        Self { cache }
    }

    /// 获取缓存的 IP（如果未过期）
    pub fn get(&self, container_name: &str) -> Option<String> {
        let result = self.cache.get(container_name);
        
        if let Some(ref ip) = result {
            debug!(
                "✅ [IP_CACHE] 缓存命中: container_name={}, ip={}",
                container_name, ip
            );
        } else {
            debug!(
                "❌ [IP_CACHE] 缓存未命中: container_name={}",
                container_name
            );
        }
        
        result
    }

    /// 缓存 IP
    pub fn insert(&self, container_name: String, ip: String) {
        debug!(
            "📝 [IP_CACHE] 写入缓存: container_name={}, ip={}",
            container_name, ip
        );
        self.cache.insert(container_name, ip);
    }

    /// 使指定容器的缓存失效
    ///
    /// 在容器重启或销毁时调用
    pub fn invalidate(&self, container_name: &str) {
        // moka 的 invalidate 方法无需检查是否存在
        self.cache.invalidate(container_name);
        info!(
            "🗑️ [IP_CACHE] 缓存已失效: container_name={}",
            container_name
        );
    }

    /// 清理所有过期条目
    ///
    /// 注意：moka 会自动清理过期条目，通常不需要手动调用
    #[allow(dead_code)]
    pub fn cleanup_expired(&self) {
        // moka 会在后台自动清理，这里强制触发一次同步清理
        self.cache.run_pending_tasks();
        debug!("🧹 [IP_CACHE] 触发过期条目清理");
    }

    /// 获取当前缓存条目数量
    #[allow(dead_code)]
    pub fn len(&self) -> u64 {
        self.cache.entry_count()
    }

    /// 检查缓存是否为空
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.cache.entry_count() == 0
    }
}

/// 默认缓存 TTL（5秒）
pub const DEFAULT_CACHE_TTL_SECONDS: u64 = 5;

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn test_cache_insert_and_get() {
        let cache = ContainerIpCache::new(10);
        
        cache.insert("container-1".to_string(), "192.168.1.1".to_string());
        
        let result = cache.get("container-1");
        assert_eq!(result, Some("192.168.1.1".to_string()));
    }

    #[test]
    fn test_cache_miss() {
        let cache = ContainerIpCache::new(10);
        
        let result = cache.get("non-existent");
        assert_eq!(result, None);
    }

    #[test]
    fn test_cache_invalidate() {
        let cache = ContainerIpCache::new(10);
        
        cache.insert("container-1".to_string(), "192.168.1.1".to_string());
        cache.invalidate("container-1");
        
        let result = cache.get("container-1");
        assert_eq!(result, None);
    }

    #[test]
    fn test_cache_ttl_expiration() {
        // 使用 1 秒 TTL 测试过期
        let cache = ContainerIpCache::new(1);
        
        cache.insert("container-1".to_string(), "192.168.1.1".to_string());
        
        // 立即获取应该命中
        assert!(cache.get("container-1").is_some());
        
        // 等待超过 TTL
        sleep(Duration::from_millis(1500));
        
        // 过期后应该返回 None
        assert!(cache.get("container-1").is_none());
    }
}
