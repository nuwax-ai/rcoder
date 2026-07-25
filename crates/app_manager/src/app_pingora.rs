//! UserApp Pingora backend 注册管理（从 service.rs 拆出，extension-impl）。
//!
//! register / unregister / rebuild_pingora_backends（Docker 模式为主）。


use tracing::{info, warn};

use container_runtime_api::{ExposeType as RtExposeType, HttpExpose};
use shared_types::ServiceType;

use super::config::AppAccessMode;
use super::models::*;
use super::utils::*;
use super::service::AppService;

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
        // backend host：Docker 用 container_ip；K8s 用 ClusterIP Service FQDN（container_ip 为空）
        let backend_host = match self.config.access_mode {
            AppAccessMode::Docker => {
                if container_ip.is_empty() {
                    warn!(
                        "[APP] Docker mode container_ip empty, skip pingora backend registration: {}",
                        app_id
                    );
                    return vec![];
                }
                container_ip.to_string()
            }
            AppAccessMode::Kubernetes => {
                let cluster_domain = shared_types::get_k8s_cluster_domain();
                format!(
                    "{}-{}-svc.{}.svc.{}",
                    ServiceType::UserApp.container_prefix(),
                    app_id,
                    self.config.namespace,
                    cluster_domain
                )
            }
        };
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
            info!("[APP] pingora backend unregistered: {} ports={:?}", app_id, ports);
        }
    }


    /// 启动时重建 Pingora backends（K8s Pingora 模式，修复重启后 pingora_ports 内存态丢失）。
    /// 从集群列出所有托管 app，按 expose_type（Deployment annotation 还原）重新注册 HTTP 端口的 backend。
    pub(crate) async fn rebuild_pingora_backends(&self) -> AppResult<()> {
        // pingora 未配置（proxy_config 未配）→ 无 backend 可注册；显式说明，避免"0 个 app"被误读为"集群无应用"
        if self.pingora.is_none() {
            info!("[APP] pingora disabled (no proxy_config), skip backends rebuild");
            return Ok(());
        }
        let statuses = self
            .runtime
            .list_deployments()
            .await
            .map_err(|e| map_runtime_error("[APP] rebuild list_deployments failed", e))?;
        let mut count = 0;
        for status in &statuses {
            let http_ports: Vec<u16> = status
                .ports
                .iter()
                .filter(|p| p.expose_type == RtExposeType::Http)
                .map(|p| p.port)
                .collect();
            if http_ports.is_empty() {
                continue;
            }
            // register 内部按 access_mode 选 host（K8s=svc FQDN）；container_ip 传空（K8s 不用）
            let registered = self
                .register_pingora_backends(&status.app_id, &http_ports, "")
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
