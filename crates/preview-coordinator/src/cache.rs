//! `/proxy/{port}` 预览路由解析缓存（正/负双 TTL；存储错误不缓存）。
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use shared_types::PreviewRouteResolution;

#[derive(Debug)]
struct Entry {
    resolution: CachedResolution,
    expires_at: Instant,
}

/// 可缓存的解析结果（Unavailable 不可缓存——降级语义不带记忆）。
#[derive(Debug, Clone)]
enum CachedResolution {
    Hit(PreviewRouteResolution),
    Negative,
}

#[derive(Debug, Default)]
pub struct RouteCache {
    entries: Mutex<HashMap<u16, Entry>>,
    positive_ttl: Duration,
    negative_ttl: Duration,
}

impl RouteCache {
    pub fn new(positive_secs: u64, negative_secs: u64) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            positive_ttl: Duration::from_secs(positive_secs),
            negative_ttl: Duration::from_secs(negative_secs),
        }
    }

    pub fn get(&self, port: u16) -> Option<PreviewRouteResolution> {
        let entries = self.entries.lock().ok()?;
        match entries.get(&port)? {
            Entry {
                resolution: CachedResolution::Hit(res),
                expires_at,
            } if expires_at > &Instant::now() => Some(res.clone()),
            Entry {
                resolution: CachedResolution::Negative,
                expires_at,
            } if expires_at > &Instant::now() => Some(PreviewRouteResolution::NotPreview),
            _ => None,
        }
    }

    /// 命中（正缓存）。TTL 不超过剩余有效期（快到期条目不续成满 TTL）。
    pub fn put_positive(&self, port: u16, resolution: PreviewRouteResolution) {
        let now = Instant::now();
        let expires_at = now + self.positive_ttl;
        if let Ok(mut entries) = self.entries.lock() {
            // 已有更晚到期的负缓存/正缓存不回退覆盖
            if let Some(existing) = entries.get(&port)
                && existing.expires_at > expires_at
            {
                return;
            }
            entries.insert(
                port,
                Entry {
                    resolution: CachedResolution::Hit(resolution),
                    expires_at,
                },
            );
        }
    }

    /// 未命中（负缓存）——仅在权威库确认"无活跃实例"后写入。
    pub fn put_negative(&self, port: u16) {
        if let Ok(mut entries) = self.entries.lock() {
            let now = Instant::now();
            let expires_at = now + self.negative_ttl;
            if let Some(existing) = entries.get(&port)
                && existing.expires_at > expires_at
            {
                return;
            }
            entries.insert(
                port,
                Entry {
                    resolution: CachedResolution::Negative,
                    expires_at,
                },
            );
        }
    }

    /// 端口身份变化（停止/实例替换）时的即时失效。
    pub fn invalidate(&self, port: u16) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(&port);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_then_expire_then_negative() {
        let cache = RouteCache::new(60, 60);
        assert!(cache.get(4000).is_none());
        cache.put_positive(
            4000,
            PreviewRouteResolution::Forward {
                instance_id: "i1".into(),
                port: 4000,
                host_ip: "10.0.0.2".into(),
            },
        );
        assert!(matches!(
            cache.get(4000),
            Some(PreviewRouteResolution::Forward { .. })
        ));
        cache.invalidate(4000);
        assert!(cache.get(4000).is_none());
        cache.put_negative(4000);
        assert!(matches!(
            cache.get(4000),
            Some(PreviewRouteResolution::NotPreview)
        ));
    }

    #[test]
    fn near_expiry_entry_is_not_extended_by_shorter_negative() {
        // 正缓存到期时间更晚时，短的负缓存不覆盖（TTL 不超过剩余有效期的变体）
        let cache = RouteCache::new(60, 1);
        cache.put_positive(
            4000,
            PreviewRouteResolution::Local {
                instance_id: "i".into(),
                port: 4000,
            },
        );
        cache.put_negative(4000);
        // 仍命中正缓存
        assert!(matches!(
            cache.get(4000),
            Some(PreviewRouteResolution::Local { .. })
        ));
    }
}
