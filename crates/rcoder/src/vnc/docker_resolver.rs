//! 基于 DockerManager 的 VNC 后端解析器
//!
//! 通过 docker_manager 的全局实例动态查询容器 IP，
//! 支持带 TTL 缓存以减少 Docker API 调用。
//!
//! ## 设计说明
//!
//! 本模块实现了 `VncBackendResolver` trait，提供异步解析 VNC 后端的能力。
//!
//! **当前状态**: 作为备选方案保留，未被主动使用。
//!
//! **原因**: Pingora 的 `upstream_peer` 方法是同步的，无法直接调用异步 Docker API。
//! 当前系统使用 `vnc_sync` 定时同步任务来维护 VNC 后端映射。
//!
//! **未来用途**: 如果 Pingora 支持异步上游解析，或使用其他支持异步的代理服务，
//! 可以直接使用此解析器。

use async_trait::async_trait;
use moka::future::Cache;
use rcoder_proxy::{VncBackendInfo, VncBackendResolver, VncResolveError};
use std::time::Duration;
use tracing::{debug, info, warn};

/// noVNC 默认端口
const NOVNC_DEFAULT_PORT: u16 = 6080;

/// 默认缓存 TTL（5 秒）
const DEFAULT_CACHE_TTL_SECS: u64 = 5;

/// 基于 DockerManager 的 VNC 后端解析器（带缓存）
///
/// 使用 moka 缓存减少 Docker API 查询频率。
/// 缓存 TTL 默认 5 秒，确保容器 IP 变化能及时更新。
///
/// ## 注意
///
/// 此结构体目前作为备选方案保留，未被主动使用。
/// 原因是 Pingora 的同步接口限制，详见模块文档。
#[allow(dead_code)]
pub struct CachedDockerResolver {
    /// 缓存：user_id -> VncBackendInfo
    cache: Cache<String, VncBackendInfo>,
}

impl Default for CachedDockerResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl CachedDockerResolver {
    /// 创建带默认 TTL（5 秒）的解析器
    pub fn new() -> Self {
        Self::with_ttl(Duration::from_secs(DEFAULT_CACHE_TTL_SECS))
    }

    /// 创建带自定义 TTL 的解析器
    pub fn with_ttl(ttl: Duration) -> Self {
        let cache = Cache::builder()
            .time_to_live(ttl)
            .max_capacity(10_000) // 最多缓存 10000 个用户
            .build();

        info!(
            "🔧 [VNC_RESOLVER] 创建 CachedDockerResolver: TTL={}s",
            ttl.as_secs()
        );

        Self { cache }
    }

    /// 直接从 DockerManager 查询容器信息（不走缓存）
    async fn query_docker(&self, user_id: &str) -> Result<VncBackendInfo, VncResolveError> {
        let docker_manager = docker_manager::global::get_global_docker_manager()
            .await
            .map_err(|e| {
                warn!("[VNC_RESOLVER] Failed to get DockerManager: {}", e);
                VncResolveError::QueryFailed(format!("Failed to get DockerManager: {}", e))
            })?;

        // ComputerAgentRunner 模式：使用 user_id 作为容器标识
        let container_info = docker_manager
            .get_user_container_info(user_id)
            .await
            .map_err(|e| {
                warn!(
                    "⚠️ [VNC_RESOLVER] 查询容器信息失败: user_id={}, error={}",
                    user_id, e
                );
                VncResolveError::QueryFailed(format!("查询容器信息失败: {}", e))
            })?
            .ok_or_else(|| {
 debug!("[VNC_RESOLVER] containernot found: user_id={}", user_id);
                VncResolveError::ContainerNotFound(user_id.to_string())
            })?;

        // 检查容器状态
        let is_running = container_info.status.to_lowercase() == "running";
        if !is_running {
            warn!(
                "⚠️ [VNC_RESOLVER] 容器未运行: user_id={}, status={}",
                user_id, container_info.status
            );
        }

        let info = VncBackendInfo::new(
            container_info.container_ip.clone(),
            NOVNC_DEFAULT_PORT,
            is_running,
        );

        debug!(
            "✅ [VNC_RESOLVER] 解析成功: user_id={} -> {}:{} (running={})",
            user_id, info.container_ip, info.vnc_port, info.is_running
        );

        Ok(info)
    }
}

#[async_trait]
impl VncBackendResolver for CachedDockerResolver {
    async fn resolve(&self, user_id: &str) -> Result<VncBackendInfo, VncResolveError> {
        // 先尝试从缓存获取
        if let Some(cached) = self.cache.get(user_id).await {
            debug!(
                "🎯 [VNC_RESOLVER] 缓存命中: user_id={} -> {}",
                user_id, cached.container_ip
            );
            return Ok(cached);
        }

        // 缓存未命中，查询 Docker
        debug!(
            "🔍 [VNC_RESOLVER] 缓存未命中，查询 Docker: user_id={}",
            user_id
        );
        let info = self.query_docker(user_id).await?;

        // 写入缓存
        self.cache.insert(user_id.to_string(), info.clone()).await;

        Ok(info)
    }

    async fn exists(&self, user_id: &str) -> bool {
        // 先检查缓存
        if self.cache.get(user_id).await.is_some() {
            return true;
        }

        // 缓存未命中，尝试解析
        self.resolve(user_id).await.is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_resolver() {
        let resolver = CachedDockerResolver::default();
        // 仅验证创建成功
        assert!(std::mem::size_of_val(&resolver) > 0);
    }

    #[test]
    fn test_custom_ttl() {
        let resolver = CachedDockerResolver::with_ttl(Duration::from_secs(10));
        assert!(std::mem::size_of_val(&resolver) > 0);
    }
}
