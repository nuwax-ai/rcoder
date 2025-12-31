//! 容器 IP 缓存模块
//!
//! 提供带 TTL 的容器 IP 缓存，避免频繁调用 Docker API。
//! 缓存使用 `container_name` 作为键（稳定标识符，重启后不变）。

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// 缓存条目
struct CacheEntry {
    ip: String,
    cached_at: Instant,
}

/// 带 TTL 的容器 IP 缓存
pub struct ContainerIpCache {
    cache: RwLock<HashMap<String, CacheEntry>>,
    ttl: Duration,
}

impl ContainerIpCache {
    /// 创建缓存实例
    ///
    /// # 参数
    /// * `ttl_seconds` - 缓存过期时间（秒）
    pub fn new(ttl_seconds: u64) -> Self {
        info!("🗄️ [IP_CACHE] 初始化容器 IP 缓存: TTL={}秒", ttl_seconds);
        Self {
            cache: RwLock::new(HashMap::new()),
            ttl: Duration::from_secs(ttl_seconds),
        }
    }

    /// 获取缓存的 IP（如果未过期）
    pub fn get(&self, container_name: &str) -> Option<String> {
        let cache = self.cache.read().ok()?;
        cache.get(container_name).and_then(|entry| {
            if entry.cached_at.elapsed() < self.ttl {
                debug!(
                    "✅ [IP_CACHE] 缓存命中: container_name={}, ip={}",
                    container_name, entry.ip
                );
                Some(entry.ip.clone())
            } else {
                debug!(
                    "⏰ [IP_CACHE] 缓存已过期: container_name={}",
                    container_name
                );
                None
            }
        })
    }

    /// 缓存 IP
    pub fn insert(&self, container_name: String, ip: String) {
        if let Ok(mut cache) = self.cache.write() {
            debug!(
                "📝 [IP_CACHE] 写入缓存: container_name={}, ip={}",
                container_name, ip
            );
            cache.insert(
                container_name,
                CacheEntry {
                    ip,
                    cached_at: Instant::now(),
                },
            );
        }
    }

    /// 使指定容器的缓存失效
    ///
    /// 在容器重启或销毁时调用
    pub fn invalidate(&self, container_name: &str) {
        if let Ok(mut cache) = self.cache.write() {
            if cache.remove(container_name).is_some() {
                info!(
                    "🗑️ [IP_CACHE] 缓存已失效: container_name={}",
                    container_name
                );
            }
        }
    }

    /// 清理所有过期条目（可定期调用）
    #[allow(dead_code)]
    pub fn cleanup_expired(&self) {
        if let Ok(mut cache) = self.cache.write() {
            let before = cache.len();
            cache.retain(|_, entry| entry.cached_at.elapsed() < self.ttl);
            let removed = before - cache.len();
            if removed > 0 {
                debug!("🧹 [IP_CACHE] 清理过期条目: {}", removed);
            }
        }
    }
}

/// 默认缓存 TTL（5秒）
pub const DEFAULT_CACHE_TTL_SECONDS: u64 = 5;
