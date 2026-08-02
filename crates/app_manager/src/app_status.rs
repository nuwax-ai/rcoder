//! UserApp 运行时状态 + 访问地址构建（从 service.rs 拆出，extension-impl）。
//!
//! fetch_runtime_status* / ensure_app_exists / build_runtime_info / build_access_info。

use tracing::{info, warn};

use container_runtime_api::{DeploymentStatus, ExposeType as RtExposeType, HttpExpose};
use shared_types::ServiceType;

use super::config::AppAccessMode;
use super::models::*;
use super::service::AppService;
use super::utils::*;

impl AppService {
    /// 实时查询单个应用运行时状态（None 表示不存在）
    pub(crate) async fn fetch_runtime_status(&self, app_id: &str) -> Option<DeploymentStatus> {
        match self.runtime.get_deployment_status(app_id).await {
            Ok(s) => s,
            Err(e) => {
                warn!("[APP] query runtime status failed app_id={}: {}", app_id, e);
                None
            }
        }
    }

    /// 实时查状态，精确区分两种"查不到"：Ok(None)=集群中真不存在 → "应用不存在"(→404)；
    /// Err=API Server 不可达/RBAC 拒绝 → "查询应用状态失败"(→500)。
    ///
    /// 供需要精确错误分类的读路径（get_app/get_app_stats/ensure_app_exists）使用，
    /// 替代会塌缩错误的 `fetch_runtime_status`（后者仅供 create_app 这类 None 可接受的场景）。
    /// 若误用 fetch_runtime_status，瞬时 API 错误会被当成"应用不存在"→404，触发 Java 误重建。
    pub(crate) async fn fetch_runtime_status_or_err(
        &self,
        app_id: &str,
    ) -> AppResult<DeploymentStatus> {
        match self.runtime.get_deployment_status(app_id).await {
            Ok(Some(s)) => Ok(s),
            Ok(None) => Err(AppOperationError::NotFound(format!(
                "app does not exist: {app_id}"
            ))),
            Err(e) => {
                warn!("[APP] query app status failed app_id={}: {}", app_id, e);
                Err(AppOperationError::Backend(format!(
                    "failed to query app status: {e}"
                )))
            }
        }
    }

    /// 确认 app 存在（集群中有 Deployment/容器），不存在返回"应用不存在"错误。
    /// 调用方（start/stop/restart）据此返回 404，方便 Java 区分并触发 create 重建，
    /// 而非收到 generic 500 误以为系统故障。
    pub(crate) async fn ensure_app_exists(&self, app_id: &str) -> AppResult<()> {
        self.fetch_runtime_status_or_err(app_id).await.map(|_| ())
    }

    /// DeploymentStatus → AppRuntimeInfo（含访问地址构建 + conditions 派生）
    pub(crate) fn build_runtime_info(&self, status: DeploymentStatus) -> AppRuntimeInfo {
        let conditions = derive_conditions(&status);
        let health = health_from_status(&status);

        // Pingora 模式（不论 Docker/K8s）：runtime status 只含 TCP（HTTP 端口无 binding），
        // 从 pingora_ports 补全 HTTP 端口，保证 get 路径的 ports/access 与 create 一致。
        // Gateway 模式：K8s status.ports 已含 HTTP（HTTPRoute backendRef），无需补。
        // ⚠️ 重启风险（pingora_ports 内存态丢失，已知限制）：
        //   - Docker：HTTP 端口补不出 → access.external.http = null（Java 可感知降级）
        //   - K8s Pingora：status.ports（containerPort）仍含 HTTP → access 返有效 /proxy/apps/{app_id}/{port}，
        //     但 Pingora backend 未重注册 → 访问 404（静默坏路径）。根治：启动从 containerPorts 重建 backends（TODO）
        let ports = if self.config.http_expose == HttpExpose::Pingora {
            let mut merged = status.ports.clone();
            if let Some(http_list) = self.pingora_ports.get(&status.app_id) {
                let http_ports: Vec<u16> = http_list.value().clone();
                // drop Ref guard，避免后续借用 self 时持有 DashMap 读锁
                drop(http_list);
                for hp in http_ports {
                    if !merged.iter().any(|p| p.port == hp) {
                        merged.push(AppPortStatus {
                            name: format!("http-{hp}"),
                            port: hp,
                            expose_type: RtExposeType::Http,
                            external_port: None,
                        });
                    }
                }
            }
            merged
        } else {
            status.ports
        };

        let access = self.build_access_info(&status.app_id, &ports);
        AppRuntimeInfo {
            status: phase_to_status(&status.phase),
            access,
            app_id: status.app_id,
            phase: status.phase,
            message: status.message,
            replicas: status.replicas,
            ready_replicas: status.ready_replicas,
            restart_count: status.restart_count,
            pod_ip: status.pod_ip,
            node: status.node,
            started_at: status.started_at,
            ports,
            conditions,
            health,
            resource_version: status.resource_version,
            recycle_enabled: status.recycle_enabled,
            idle_timeout_seconds: status.idle_timeout_seconds,
            created_at: status.created_at,
        }
    }

    /// 重建活动状态内存态(rcoder 重启后调用):`list_deployments` →
    /// replicas==0 的 `mark_stopped`(支持流量唤醒识别 stopped app);replicas>0 的 `seed_accessed`(种
    /// last_accessed=now,给 Running app 完整 grace 周期,避免重启后立刻被回收)。失败向上传播。
    pub(crate) async fn rebuild_stopped_apps(&self) -> AppResult<()> {
        let deploys = self.runtime.list_deployments().await.map_err(|e| {
            map_runtime_error("[APP] list_deployments failed (rebuild_stopped_apps)", e)
        })?;
        let mut stopped = 0u32;
        let mut running = 0u32;
        for s in deploys {
            if s.replicas == 0 {
                self.activity.mark_stopped(&s.app_id);
                stopped += 1;
            } else {
                self.activity.seed_accessed(&s.app_id);
                running += 1;
            }
        }
        info!(
            "[APP] activity rebuild: {} stopped, {} running apps seeded",
            stopped, running
        );
        Ok(())
    }

    /// 构建访问信息（按 `http_expose` 决定 HTTP path；一律只返 path，host 由 Java 拼）
    pub(crate) fn build_access_info(&self, app_id: &str, ports: &[AppPortStatus]) -> AccessInfo {
        let http_port = ports.iter().find(|p| p.expose_type == RtExposeType::Http);

        // 一律只返 path，host 由 Java 拼（Java 必然已知 RCoder / gateway 入口，否则访问不了）：
        // - Pingora 模式（默认，两后端统一）：/proxy/apps/{app_id}/{port}
        // - Gateway 模式（K8s 可选）：/apps/{app_id}
        // TCP 初期不对外（external.tcp 空）；internal 始终给 ClusterIP FQDN / 容器名。
        let http_url = match self.config.http_expose {
            HttpExpose::Pingora => http_port.map(|p| format!("/proxy/apps/{}/{}", app_id, p.port)),
            HttpExpose::Gateway => http_port.map(|_| format!("/apps/{}", app_id)),
        };

        // internal domain：K8s = ClusterIP Service FQDN；Docker = 容器名（= 资源名）
        let (domain, short_domain) = match self.config.access_mode {
            AppAccessMode::Docker => {
                let name = format!("{}-{}", ServiceType::UserApp.container_prefix(), app_id);
                (name.clone(), name)
            }
            AppAccessMode::Kubernetes => {
                let cluster_domain = shared_types::get_k8s_cluster_domain();
                let svc = format!("{}-{}-svc", ServiceType::UserApp.container_prefix(), app_id);
                (
                    format!("{}.{}.svc.{}", svc, self.config.namespace, cluster_domain),
                    format!("{}.{}", svc, self.config.namespace),
                )
            }
        };

        AccessInfo {
            external: ExternalAccess {
                http: http_url,
                tcp: vec![], // TCP 初期不对外
            },
            internal: InternalAccess {
                domain,
                short_domain,
                ports: ports
                    .iter()
                    .map(|p| InternalPort {
                        name: p.name.clone(),
                        port: p.port,
                    })
                    .collect(),
            },
        }
    }
}
