//! 基于 Pingora 的代理服务模块
//!
//! 提供使用 Cloudflare Pingora 库实现的高性能反向代理服务，支持负载均衡。
//!
//! ## 模块结构
//!
//! - [`types`] - 类型定义（PerPortMetrics, ProxyMetrics, TrackingCtx 等）
//! - [`utils`] - 工具函数（normalize_path, rewrite_uri, set_common_headers 等）
//! - [`handlers`] - 请求处理函数（VNC, ttyd, audio, IME, API proxy, port proxy）
//! - [`proxy_http`] - `ProxyHttp for PortProxy` 实现（请求生命周期 filter/peer/response）
//! - [`backends`] - `PingoraProxyService` 后端映射管理（port/app/vnc/project + health check）
//!
//! 本模块（mod.rs）仅保留 struct 定义 + 构造（new/builders/create_pingora_proxy）+ Clone。

mod backends;
mod dispatch;
mod proxy_http;

pub mod handlers;
pub mod types;
pub mod utils;

use anyhow::Result;
use arc_swap::{ArcSwap, ArcSwapOption};
use dashmap::DashMap;
use matchit::Router;
use std::collections::HashMap;
use std::sync::Arc;

// 导入 shared_types 以使用 ModelProviderConfig
use shared_types::ModelProviderConfig;

use crate::config::ProxyConfig;
use crate::router::{RouteType, create_router};

// 重新导出类型，保持向后兼容
pub use types::*;
pub use utils::*;

/// 基于 Pingora 的端口反向代理服务
pub struct PingoraProxyService {
    config: ProxyConfig,
    backends: Arc<ArcSwap<HashMap<u16, String>>>,
    /// 负载均衡算法选择
    pub use_round_robin: bool,
    /// 指标
    pub metrics: Arc<ProxyMetrics>,
    /// 后端健康状态缓存
    pub health_map: Arc<ArcSwap<HashMap<u16, HealthInfo>>>,
    /// VNC 后端映射: user_id/pod_id -> container_ip
    /// 用于 /computer/vnc/{user_id}/{project_id} 路由
    pub vnc_backends: Arc<DashMap<String, String>>,
    /// Project 后端映射: project_id -> container_ip
    /// 用于 /web/ttyd/{user_id}/{project_id} 路由（共享容器场景）
    pub project_backends: Arc<DashMap<String, String>>,
    /// app 后端映射: (app_id, port) -> host
    /// 用于 /proxy/apps/{app_id}/{port} 路由（app_manager 部署的应用，按 app_id+port 路由避免同端口冲突）
    pub app_backends: Arc<DashMap<(String, u16), String>>,
    /// 🔒 API 密钥管理器: service_name -> ModelProviderConfig
    /// 用于 /api/{service_name}/{*path} 路由
    pub api_key_manager: Arc<DashMap<String, ModelProviderConfig>>,
    /// 🔒 API Key 鉴权配置（可选，用于 VNC 等路由的鉴权，使用 ArcSwap 实现无锁读取）
    pub api_key_config: Option<Arc<ArcSwap<shared_types::ApiKeyAuthConfig>>>,
    /// 容器查找服务（统一数据源）
    container_lookup: Option<Arc<dyn shared_types::ContainerLookup>>,
    /// UserApp 访问追踪（Pingora 热路径 touch 记录最近访问，供闲置回收判断）
    access_tracker: Option<Arc<dyn shared_types::AppAccessTracker>>,
    /// UserApp 流量唤醒（stopped app 收到请求时 hold-and-wait 拉起）
    wake_control: Option<Arc<dyn shared_types::AppWakeControl>>,
    /// userApp 运行容器 IPv4 解析（Docker 模式；ArcSwap 槽——启动后经
    /// [`Self::set_app_runtime_ip_resolver`] 回填，PortProxy 共享同槽实时见）
    pub(crate) app_runtime_ip_slot: Arc<ArcSwapOption<Arc<dyn shared_types::AppRuntimeIpResolver>>>,
}

/// 为了兼容现有接口，我们保留原来的 PortProxyService 别名
pub type PortProxyService = PingoraProxyService;

/// Pingora 代理实现
pub struct PortProxy {
    backends: Arc<ArcSwap<HashMap<u16, String>>>,
    #[allow(dead_code)]
    default_backend_port: u16,
    backend_host: String,
    /// 负载均衡算法选择
    pub use_round_robin: bool,
    /// 指标
    pub metrics: Arc<ProxyMetrics>,
    /// VNC 后端映射: user_id/pod_id -> container_ip
    vnc_backends: Arc<DashMap<String, String>>,
    /// Project 后端映射: project_id -> container_ip
    project_backends: Arc<DashMap<String, String>>,
    /// app 后端映射: (app_id, port) -> host（/proxy/apps/{app_id}/{port} 路由）
    app_backends: Arc<DashMap<(String, u16), String>>,
    /// 路由表
    router: Router<RouteType>,
    /// 🔒 API 密钥管理器: service_name -> ModelProviderConfig
    api_key_manager: Arc<DashMap<String, ModelProviderConfig>>,
    /// 🔒 API Key 鉴权配置（可选，用于 VNC 等路由的鉴权，使用 ArcSwap 实现无锁读取）
    #[allow(dead_code)]
    api_key_config: Option<Arc<ArcSwap<shared_types::ApiKeyAuthConfig>>>,
    /// 容器查找服务（统一数据源）
    container_lookup: Option<Arc<dyn shared_types::ContainerLookup>>,
    /// UserApp 访问追踪 + 流量唤醒（/proxy/apps/* 路由用）
    access_tracker: Option<Arc<dyn shared_types::AppAccessTracker>>,
    wake_control: Option<Arc<dyn shared_types::AppWakeControl>>,
    /// userApp 运行容器 IPv4 解析槽（与 PingoraProxyService 共享同一 Arc）
    pub(crate) app_runtime_ip_slot: Arc<ArcSwapOption<Arc<dyn shared_types::AppRuntimeIpResolver>>>,
}

impl PingoraProxyService {
    /// 创建新的 Pingora 代理服务
    pub fn new(config: ProxyConfig) -> Self {
        let mut backends = HashMap::new();
        // 添加默认后端
        backends.insert(config.default_backend_port, config.backend_host.clone());

        Self {
            config,
            backends: Arc::new(ArcSwap::from_pointee(backends)),
            use_round_robin: true, // 默认使用轮询算法
            metrics: Arc::new(ProxyMetrics::default()),
            health_map: Arc::new(ArcSwap::from_pointee(HashMap::new())),
            vnc_backends: Arc::new(DashMap::new()),
            project_backends: Arc::new(DashMap::new()),
            app_backends: Arc::new(DashMap::new()),
            api_key_manager: Arc::new(DashMap::new()),
            api_key_config: None, // 默认不启用 API Key 鉴权
            container_lookup: None,
            access_tracker: None,
            wake_control: None,
            app_runtime_ip_slot: Arc::new(ArcSwapOption::from(None)),
        }
    }

    /// 回填 userApp 运行容器 IPv4 解析器（启动后调用——main 侧 RuntimeManager
    /// 就绪晚于 Pingora 启动；PortProxy 共享槽，无锁生效）。
    pub fn set_app_runtime_ip_resolver(
        &self,
        resolver: Arc<dyn shared_types::AppRuntimeIpResolver>,
    ) {
        self.app_runtime_ip_slot.store(Some(Arc::new(resolver)));
    }

    /// 设置容器查找服务（统一数据源）
    pub fn with_container_lookup(
        mut self,
        container_lookup: Arc<dyn shared_types::ContainerLookup>,
    ) -> Self {
        self.container_lookup = Some(container_lookup);
        self
    }

    /// 设置 UserApp 访问追踪（闲置回收的 HTTP 访问信号源）
    pub fn with_access_tracker(mut self, tracker: Arc<dyn shared_types::AppAccessTracker>) -> Self {
        self.access_tracker = Some(tracker);
        self
    }

    /// 设置 UserApp 流量唤醒控制
    pub fn with_wake_control(mut self, wc: Arc<dyn shared_types::AppWakeControl>) -> Self {
        self.wake_control = Some(wc);
        self
    }

    /// 设置负载均衡算法
    pub fn with_load_balancing(mut self, use_round_robin: bool) -> Self {
        self.use_round_robin = use_round_robin;
        self
    }

    /// 设置共享的 API 密钥管理器
    ///
    /// 这个方法允许从外部传入一个共享的 DashMap，使 agent_runner 和 Pingora
    /// 能够共享 API 密钥配置。
    pub fn with_api_key_manager(
        mut self,
        api_key_manager: Arc<DashMap<String, ModelProviderConfig>>,
    ) -> Self {
        self.api_key_manager = api_key_manager;
        self
    }

    /// 设置 API Key 鉴权配置（builder 模式）
    ///
    /// 传入共享的 API Key 配置，使 Pingora 层也能进行 API Key 验证。
    /// 配置将被传递给 PortProxy，用于在 upstream_request_filter 中验证请求。
    /// 使用 ArcSwap 实现无锁读取，提升并发性能。
    pub fn with_api_key_config(
        mut self,
        config: Arc<ArcSwap<shared_types::ApiKeyAuthConfig>>,
    ) -> Self {
        self.api_key_config = Some(config);
        self
    }

    /// 创建 PortProxy 实例
    pub fn create_pingora_proxy(&self) -> Result<PortProxy, crate::ProxyError> {
        let router = create_router()?;

        Ok(PortProxy {
            backends: self.backends.clone(),
            default_backend_port: self.config.default_backend_port,
            backend_host: self.config.backend_host.clone(),
            use_round_robin: self.use_round_robin,
            metrics: self.metrics.clone(),
            vnc_backends: self.vnc_backends.clone(),
            project_backends: self.project_backends.clone(),
            app_backends: self.app_backends.clone(),
            router,
            api_key_manager: self.api_key_manager.clone(),
            api_key_config: self.api_key_config.clone(),
            container_lookup: self.container_lookup.clone(),
            access_tracker: self.access_tracker.clone(),
            wake_control: self.wake_control.clone(),
            app_runtime_ip_slot: self.app_runtime_ip_slot.clone(),
        })
    }
}

impl Clone for PingoraProxyService {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            backends: self.backends.clone(),
            use_round_robin: self.use_round_robin,
            metrics: self.metrics.clone(),
            health_map: self.health_map.clone(),
            vnc_backends: self.vnc_backends.clone(),
            project_backends: self.project_backends.clone(),
            app_backends: self.app_backends.clone(),
            api_key_manager: self.api_key_manager.clone(),
            api_key_config: self.api_key_config.clone(),
            container_lookup: self.container_lookup.clone(),
            access_tracker: self.access_tracker.clone(),
            wake_control: self.wake_control.clone(),
            app_runtime_ip_slot: self.app_runtime_ip_slot.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProxyConfig;

    #[test]
    fn test_service_creation() {
        let config = ProxyConfig::default();
        let service = PingoraProxyService::new(config);
        assert!(service.use_round_robin);
    }

    #[test]
    fn test_service_clone() {
        let config = ProxyConfig::default();
        let service = PingoraProxyService::new(config);
        let cloned = service.clone();
        assert_eq!(service.use_round_robin, cloned.use_round_robin);
    }

    #[tokio::test]
    async fn test_vnc_backend_management() {
        let config = ProxyConfig::default();
        let service = PingoraProxyService::new(config);

        // 添加 VNC 后端
        service.add_vnc_backend("user1", "192.168.1.100");
        assert!(service.has_vnc_backend("user1"));
        assert_eq!(
            service.get_vnc_backend("user1"),
            Some("192.168.1.100".to_string())
        );
        assert_eq!(service.vnc_backend_count(), 1);

        // 列出所有后端
        let backends = service.list_vnc_backends();
        assert_eq!(backends.len(), 1);
        assert_eq!(backends.get("user1"), Some(&"192.168.1.100".to_string()));

        // 移除后端
        let removed = service.remove_vnc_backend("user1");
        assert_eq!(removed, Some("192.168.1.100".to_string()));
        assert!(!service.has_vnc_backend("user1"));
        assert_eq!(service.vnc_backend_count(), 0);
    }

    #[tokio::test]
    async fn test_backend_count() {
        let config = ProxyConfig::default();
        let service = PingoraProxyService::new(config);

        assert_eq!(service.backend_count().await, 1); // 默认后端
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_backend_updates_do_not_lose_routes() {
        let service = PingoraProxyService::new(ProxyConfig::default());
        let mut tasks = Vec::new();
        for index in 0..32_u16 {
            let service = service.clone();
            tasks.push(tokio::spawn(async move {
                service
                    .add_backend(10_000 + index, format!("backend-{index}"))
                    .await;
            }));
        }
        for task in tasks {
            task.await.expect("backend update task");
        }

        let snapshot = service.list_backends().await;
        assert_eq!(snapshot.len(), 33, "default backend plus all RCU updates");
        for index in 0..32_u16 {
            assert_eq!(
                snapshot.get(&(10_000 + index)),
                Some(&format!("backend-{index}"))
            );
        }
    }
}
