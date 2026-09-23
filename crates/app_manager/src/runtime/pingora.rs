//! Userapp Pingora backend 注册管理（从 service.rs 拆出，extension-impl）。
//!
//! register / unregister / rebuild_pingora_backends（Docker 模式为主）。

use tracing::{info, warn};

use container_runtime_api::{ExposeType as RtExposeType, HttpExpose};
use shared_types::ServiceType;

use crate::config::AppAccessMode;
use crate::models::*;
use crate::service::AppService;
use crate::utils::*;

impl AppService {
    /// 为 HTTP 端口注册 Pingora backend（Pingora 模式，Docker/K8s 统一）。
    /// backend host 按后端：Docker=container_ip，K8s=ClusterIP Service FQDN（Pod 内 kube-dns 解析）。
    /// Gateway 模式不注册（HTTP 走 HTTPRoute）。
    pub(crate) async fn register_pingora_backends(
        &self,
        app_id: &str,
        http_ports: &[u16],
        container_ip: &str,
    ) -> Vec<u16> {
        // Gateway 模式 HTTP 走 HTTPRoute，不经 Pingora——跳过
        if self.config.http_expose == HttpExpose::Gateway {
            return vec![];
        }
        let Some(pingora) = &self.pingora else {
            return vec![];
        };
        // backend host：Docker 用 container_ip；K8s 用 ClusterIP Service FQDN（container_ip 为空）。
        // deploy-host：统一注册表键 = 确定性容器名（与 lookup/published 注册同源）
        #[cfg(feature = "deploy-host")]
        let backend_host = if shared_types::is_deploy_host() {
            format!("{}-{app_id}", ServiceType::Userapp.container_prefix())
        } else {
            self.access_backend_host(container_ip, app_id)
        };
        #[cfg(not(feature = "deploy-host"))]
        let backend_host = self.access_backend_host(container_ip, app_id);
        // Docker 形态 container_ip 为空时整体跳过注册（原语义：空 host 不入表）
        if backend_host.is_empty() {
            return vec![];
        }
        for port in http_ports {
            pingora.add_app_backend(app_id, *port, backend_host.clone());
        }
        if !http_ports.is_empty() {
            self.pingora_ports
                .insert(app_id.to_string(), http_ports.to_vec());
            info!(
                "[APP] pingora backend registered: {} ports={:?} -> {}",
                app_id, http_ports, backend_host
            );
        }
        http_ports.to_vec()
    }

    /// 当前实际注册的 HTTP 端口（update 的恢复/兜底依据）。
    ///
    /// 比 `DeploymentStatus.ports` 反推可靠：Docker 后端的状态 ports 只含 TCP
    /// （HTTP 走 Pingora 不做 binding），反推恒空——patch 失败恢复与 ports 缺省
    /// 兜底用它才真正注册回路由。注意须在 `unregister_pingora_backends` **之前**
    /// 调用（unregister 会移除注册表条目）。
    pub(crate) fn registered_http_ports(&self, app_id: &str) -> Vec<u16> {
        self.pingora_ports
            .get(app_id)
            .map(|entry| entry.value().clone())
            .unwrap_or_default()
    }

    /// 清理 app 曾注册的 Pingora backend（Pingora 模式）。Gateway 模式未注册过，直接返回。
    pub(crate) async fn unregister_pingora_backends(&self, app_id: &str) {
        if self.config.http_expose == HttpExpose::Gateway {
            return;
        }
        let Some(pingora) = &self.pingora else {
            return;
        };
        if let Some((_, ports)) = self.pingora_ports.remove(app_id) {
            for port in &ports {
                pingora.remove_app_backend(app_id, *port);
            }
            info!(
                "[APP] pingora backend unregistered: {} ports={:?}",
                app_id, ports
            );
        }
    }

    /// 启动时重建 Pingora backends（K8s Pingora 模式，修复重启后 pingora_ports 内存态丢失）。
    /// 从集群列出所有托管 app，按 expose_type（Deployment annotation 还原）重新注册 HTTP 端口的 backend。
    /// 按 access_mode 构造 backend host（deploy-host 的注册表键在外层分支处理）：
    /// Docker=container_ip；K8s=ClusterIP Service FQDN（Pod 内 kube-dns 解析）。
    fn access_backend_host(&self, container_ip: &str, app_id: &str) -> String {
        match self.config.access_mode {
            AppAccessMode::Docker => {
                if container_ip.is_empty() {
                    warn!(
                        "[APP] Docker mode container_ip empty, skip pingora backend registration: {}",
                        app_id
                    );
                    return String::new();
                }
                container_ip.to_string()
            }
            AppAccessMode::Kubernetes => {
                let cluster_domain = shared_types::get_k8s_cluster_domain();
                format!(
                    "{}-{}-svc.{}.svc.{}",
                    ServiceType::Userapp.container_prefix(),
                    app_id,
                    self.config.namespace,
                    cluster_domain
                )
            }
        }
    }

    pub(crate) async fn rebuild_pingora_backends(&self) -> AppResult<()> {
        // pingora 未配置（proxy_config 未配）→ 无 backend 可注册；显式说明，避免"0 个 app"被误读为"集群无应用"
        if self.pingora.is_none() {
            info!("[APP] pingora disabled (no proxy_config), skip backends rebuild");
            return Ok(());
        }
        let statuses = match self.runtime.list_deployments().await {
            Ok(statuses) => statuses,
            Err(error) if self.config.access_mode == AppAccessMode::Docker => {
                warn!(%error, "Docker app route recovery could not list containers; controls remain available");
                return Ok(());
            }
            Err(error) => {
                return Err(map_runtime_error(
                    "[APP] rebuild list_deployments failed",
                    error,
                ));
            }
        };
        let mut count = 0;
        for status in &statuses {
            let http_ports: Vec<u16> = status
                .ports
                .iter()
                .filter(|p| p.expose_type == RtExposeType::Http)
                .map(|p| p.port)
                .collect();
            if http_ports.is_empty() {
                if self.config.access_mode == AppAccessMode::Docker && status.replicas > 0 {
                    warn!(app_id = %status.app_id,
                        "Running Docker app has no valid HTTP port metadata for route recovery");
                }
                continue;
            }
            if self.config.access_mode == AppAccessMode::Docker {
                let identity = match self.metadata.store.get_application(&status.app_id).await {
                    Ok(Some(identity)) => identity,
                    Ok(None) => {
                        warn!(app_id = %status.app_id, "Docker app route recovery has no lifecycle identity");
                        continue;
                    }
                    Err(error) => {
                        warn!(app_id = %status.app_id, %error, "Docker app route recovery identity read failed");
                        continue;
                    }
                };
                if identity.state != shared_types::UserAppLifecycleState::Active
                    || status.lifecycle_id.as_deref() != Some(identity.lifecycle_id.as_str())
                {
                    warn!(app_id = %status.app_id, "Docker app route recovery lifecycle mismatch");
                    continue;
                }
                // A stopped app retains its typed HTTP metadata for explicit
                // Start/traffic wake but must not create a live proxy backend.
                if status.replicas == 0 {
                    self.pingora_ports.insert(status.app_id.clone(), http_ports);
                    continue;
                }
                #[cfg(feature = "deploy-host")]
                let needs_ip =
                    !shared_types::is_deploy_host() || shared_types::deploy_host_reach::is_direct();
                #[cfg(not(feature = "deploy-host"))]
                let needs_ip = true;
                if needs_ip && status.pod_ip.is_none() {
                    warn!(app_id = %status.app_id, "Running Docker app has no current IP for route recovery");
                    continue;
                }
            }
            let registered = self
                .register_pingora_backends(
                    &status.app_id,
                    &http_ports,
                    status.pod_ip.as_deref().unwrap_or_default(),
                )
                .await;
            if !registered.is_empty() {
                count += 1;
            }
        }
        info!(
            "[APP] pingora backends rebuilt: {count} apps ({} managed apps total in cluster)",
            statuses.len()
        );
        Ok(())
    }
}
