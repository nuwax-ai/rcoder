//! `PingoraProxyService` 的后端映射管理 —— port / app / vnc / project backends + 健康检查。
//!
//! 从 `service/mod.rs` 拆出。构造（new / with_* builders / create_pingora_proxy）与 struct 定义留在 mod.rs。
//! Rust 允许同一类型多个 `impl` 块，此处方法与 mod.rs 的构造 impl 合并到 `PingoraProxyService`。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use arc_swap::ArcSwap;
use dashmap::DashMap;
use pingora_load_balancing::{LoadBalancer, health_check, selection::RoundRobin};
use shared_types::ModelProviderConfig;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, info};

use crate::config::ProxyConfig;

use super::{HealthInfo, HealthState, PingoraProxyService};

impl PingoraProxyService {
    /// 获取后端数量
    pub async fn backend_count(&self) -> usize {
        self.backends.load().len()
    }

    /// 从请求中提取目标端口（兼容接口）
    #[allow(dead_code)]
    pub fn extract_target_port(&self, req: &axum::extract::Request) -> crate::ProxyResult<u16> {
        // 1. 首先尝试从 Path 中提取端口 (例如 /proxy/8080/path)
        let path = req.uri().path();
        if path.starts_with("/proxy/") {
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() >= 3
                && let Ok(port) = parts[2].parse::<u16>()
            {
                debug!("proxy path for port: {}", port);
                return Ok(port);
            }
        }

        // 2. 然后尝试从 URL 查询参数中获取端口 (向后兼容)
        if let Some(query) = req.uri().query() {
            for param in query.split('&') {
                if let Some((key, value)) = param.split_once('=')
                    && key == self.config.port_param
                    && let Ok(port) = value.parse::<u16>()
                {
                    debug!("URL params for port: {}", port);
                    return Ok(port);
                }
            }
        }

        // 3. 使用默认端口
        debug!("default port: {}", self.config.default_backend_port);
        Ok(self.config.default_backend_port)
    }

    /// 获取目标后端地址
    pub async fn get_target_backend(&self, port: u16) -> crate::ProxyResult<String> {
        let backends = self.backends.load();
        backends.get(&port).cloned().ok_or_else(|| {
            crate::ProxyError::Backend(format!("backend service not found for port {}", port))
        })
    }

    /// 添加后端服务
    pub async fn add_backend(&self, port: u16, host: String) {
        self.backends.rcu(|backends| {
            let mut updated = HashMap::clone(backends);
            updated.insert(port, host.clone());
            updated
        });
        info!(" proxy route: {} -> {}", port, host);
    }

    /// 移除后端服务
    pub async fn remove_backend(&self, port: u16) {
        let existed = self.backends.load().contains_key(&port);
        if existed {
            self.backends.rcu(|backends| {
                let mut updated = HashMap::clone(backends);
                updated.remove(&port);
                updated
            });
            info!("removed route: {}", port);
        }
    }

    /// 添加 app 后端（免端口代理按 (app_id, APP_ENTRY_PORT) 优先查，用于 /proxy/userapp/prod）。
    /// 同步（DashMap），与 vnc/project 一致。
    pub fn add_app_backend(&self, app_id: &str, port: u16, host: String) {
        self.app_backends
            .insert((app_id.to_string(), port), host.clone());
        info!(" app proxy route: {app_id}:{port} -> {host}");
    }

    /// 移除 app 后端（按 app_id+port）。返回是否曾存在。
    pub fn remove_app_backend(&self, app_id: &str, port: u16) -> bool {
        let removed = self
            .app_backends
            .remove(&(app_id.to_string(), port))
            .is_some();
        if removed {
            info!("removed app proxy route: {app_id}:{port}");
        }
        removed
    }

    /// 列出所有后端服务
    pub async fn list_backends(&self) -> HashMap<u16, String> {
        HashMap::clone(&self.backends.load())
    }

    /// 检查后端服务是否存在
    pub async fn has_backend(&self, port: u16) -> bool {
        self.backends.load().contains_key(&port)
    }

    /// 创建负载均衡器
    pub async fn create_load_balancer(
        &self,
        backend_list: Vec<String>,
    ) -> Result<LoadBalancer<RoundRobin>> {
        let mut lb = LoadBalancer::try_from_iter(backend_list)?;

        // 添加健康检查
        let hc = health_check::TcpHealthCheck::new();
        lb.set_health_check(hc);
        lb.health_check_frequency = Some(Duration::from_secs(5));

        Ok(lb)
    }

    /// 获取配置的只读引用
    pub fn config(&self) -> &ProxyConfig {
        &self.config
    }

    /// 获取后端映射的 Arc 引用
    pub fn backends(&self) -> Arc<ArcSwap<HashMap<u16, String>>> {
        self.backends.clone()
    }

    /// 兼容性方法：代理请求（用于与现有接口兼容）
    ///
    /// 注意：这个方法仅用于兼容性，实际的代理功能由 Pingora 服务器处理
    pub async fn proxy_request(
        &self,
        _req: axum::extract::Request,
    ) -> crate::ProxyResult<axum::response::Response> {
        // 这个方法提供兼容性，但实际的代理由 Pingora 服务器处理
        // 在实际部署中，请求会直接发送到 Pingora 监听的端口
        Err(crate::ProxyError::RequestHandling(
            "This method is only for compatibility. Actual proxy functionality is handled by Pingora server, please directly request the port Pingora is listening on".to_string()
        ))
    }

    /// 更新一次所有后端的健康状态
    ///
    /// # 并发安全性
    /// - 先克隆 backends 并快速释放锁
    /// - 不持有锁进行网络 I/O（避免死锁）
    /// - 批量更新 health_map（只获取一次锁）
    pub async fn update_health_once(&self, timeout_ms: u64) {
        // 1. 快速克隆 backends 并释放锁（避免持有锁期间 await）
        let backends_snapshot = self.backends.load_full();

        // 2. 不持有任何锁进行网络 I/O
        let mut results = HashMap::new();
        for (port, host) in backends_snapshot.iter() {
            let addr = format!("{}:{}", host, port);
            let state =
                match timeout(Duration::from_millis(timeout_ms), TcpStream::connect(&addr)).await {
                    Ok(Ok(_stream)) => HealthState::Healthy,
                    Ok(Err(_)) => HealthState::Unhealthy,
                    Err(_) => HealthState::Timeout,
                };
            results.insert(
                *port,
                HealthInfo {
                    status: state,
                    last_check: SystemTime::now(),
                },
            );
        }

        // 3. 批量更新 health_map（只获取一次写锁）
        self.health_map.store(Arc::new(results));
    }

    /// 获取所有后端的健康状态快照
    pub async fn get_health_snapshot(&self) -> HashMap<u16, HealthInfo> {
        HashMap::clone(&self.health_map.load())
    }

    /// 启动健康检查循环
    pub fn start_health_check_loop(&self, interval_secs: u64, timeout_ms: u64) {
        let svc = self.clone();
        tokio::spawn(async move {
            let interval = Duration::from_secs(interval_secs);
            loop {
                svc.update_health_once(timeout_ms).await;
                tokio::time::sleep(interval).await;
            }
        });
    }

    /// 获取健康状态快照（兼容接口）
    pub async fn health_snapshot(&self) -> HashMap<u16, HealthInfo> {
        self.get_health_snapshot().await
    }

    // ========================================================================
    // 🔧 VNC 后端管理方法
    // ========================================================================

    /// 添加 VNC 后端映射
    ///
    /// 当创建 ComputerAgentRunner 容器时调用，注册 user_id 到 container_ip 的映射
    pub fn add_vnc_backend(&self, user_id: &str, container_ip: &str) {
        self.vnc_backends
            .insert(user_id.to_string(), container_ip.to_string());
        info!(
            "Added VNC backend: user_id={} -> container_ip={}",
            user_id, container_ip
        );
    }

    /// 移除 VNC 后端映射
    ///
    /// 当销毁 ComputerAgentRunner 容器时调用
    pub fn remove_vnc_backend(&self, user_id: &str) -> Option<String> {
        let removed = self.vnc_backends.remove(user_id);
        if let Some((_, ip)) = &removed {
            info!("removed VNC route: user_id={} (was: {})", user_id, ip);
        }
        removed.map(|(_, ip)| ip)
    }

    /// 添加 Project 后端映射
    ///
    /// 用于 WebAgentRunner 容器，project_id 作为 key
    pub fn add_project_backend(&self, project_id: &str, container_ip: &str) {
        self.project_backends
            .insert(project_id.to_string(), container_ip.to_string());
        info!(
            "Added project backend: project_id={} -> container_ip={}",
            project_id, container_ip
        );
    }

    /// 移除 Project 后端映射
    ///
    /// 当销毁 WebAgentRunner 容器时调用
    pub fn remove_project_backend(&self, project_id: &str) -> Option<String> {
        let removed = self.project_backends.remove(project_id);
        if let Some((_, ip)) = &removed {
            info!(
                "removed project backend: project_id={} (was: {})",
                project_id, ip
            );
        }
        removed.map(|(_, ip)| ip)
    }

    /// 获取 VNC 后端 IP
    pub fn get_vnc_backend(&self, user_id: &str) -> Option<String> {
        self.vnc_backends.get(user_id).map(|r| r.value().clone())
    }

    /// 检查 VNC 后端是否存在
    pub fn has_vnc_backend(&self, user_id: &str) -> bool {
        self.vnc_backends.contains_key(user_id)
    }

    /// 获取所有 VNC 后端映射
    pub fn list_vnc_backends(&self) -> HashMap<String, String> {
        self.vnc_backends
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect()
    }

    /// 获取 VNC 后端数量
    pub fn vnc_backend_count(&self) -> usize {
        self.vnc_backends.len()
    }

    /// 列出所有 Project 后端映射
    ///
    /// 用于同步和清理逻辑
    pub fn list_project_backends(&self) -> HashMap<String, String> {
        self.project_backends
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect()
    }

    /// 获取 Project 后端数量
    pub fn project_backend_count(&self) -> usize {
        self.project_backends.len()
    }

    /// 查找容器 IP（统一入口）
    ///
    /// 优先使用 ContainerLookupService，回退到 vnc_backends/project_backends
    pub fn find_container_ip(
        &self,
        service_type: &shared_types::ServiceType,
        user_id: Option<&str>,
        project_id: Option<&str>,
        pod_id: Option<&str>,
    ) -> Option<String> {
        // 1. 优先使用 ContainerLookupService
        if let Some(ref lookup) = self.container_lookup {
            let result = lookup.find_container_ip(service_type, user_id, project_id, pod_id);
            if result.is_some() {
                return result;
            }
        }

        // 2. 回退到 vnc_backends/project_backends（向后兼容）
        // 优先使用 pod_id
        if let Some(pid) = pod_id
            && let Some(ip) = self.vnc_backends.get(pid)
        {
            return Some(ip.value().clone());
        }

        // 根据 ServiceType 选择路由键
        match service_type {
            shared_types::ServiceType::ComputerAgentRunner => {
                if let Some(uid) = user_id {
                    self.vnc_backends.get(uid).map(|r| r.value().clone())
                } else {
                    None
                }
            }
            shared_types::ServiceType::WebAgentRunner => {
                if let Some(pid) = project_id {
                    self.project_backends.get(pid).map(|r| r.value().clone())
                } else {
                    None
                }
            }
            // UserApp / UserAppBuilder 不走 VNC/project backend 查找:
            // UserApp 经 /proxy/{port} 的 backends map 路由;
            // UserAppBuilder 经 agent lookup(project_id)由上层解析,此处返 None
            shared_types::ServiceType::UserApp | shared_types::ServiceType::UserAppBuilder => None,
        }
    }

    // ========================================================================
    // 🔒 API 密钥管理方法
    // ========================================================================

    /// 获取 API 密钥管理器的引用（用于共享）
    pub fn get_api_key_manager(&self) -> Arc<DashMap<String, ModelProviderConfig>> {
        self.api_key_manager.clone()
    }
}
