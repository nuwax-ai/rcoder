//! Pingora 代理查询面：backend 地址解析 + ContainerLookup 实现（从 adapter.rs 拆出）
//!
//! ContainerLookup 供 rcoder-proxy 的 ttyd/VNC 路由解析容器 IP（K8s 走 Service
//! FQDN、Docker 走 IP）；与 backend 枚举的转发实现同语义。

use shared_types::{ContainerBasicInfo, ServiceType};
use tracing::debug;

use super::adapter::ProjectAdapter;

// ========== pingora backend 地址解析 ==========

impl ProjectAdapter {
    /// 解析 pingora 反向代理的 backend 地址。
    ///
    /// - K8s:headless Service FQDN(`{container_name}-svc.{ns}.svc.{domain}`),经 K8s DNS
    ///   解析;Pod 重建后 Service selector 选到新 Pod,DNS 自动指向新 IP,客户端重连即
    ///   恢复,无需 rcoder 重注册/重查(与 `register_vnc_backend` 的 vnc_backends 对齐)。
    /// - Docker:容器 IP(直连)。
    pub(crate) fn resolve_backend_addr(&self, info: &ContainerBasicInfo) -> String {
        if shared_types::is_kubernetes_runtime() {
            shared_types::build_k8s_service_fqdn(
                &info.container_name,
                &self.namespace,
                &self.cluster_domain,
            )
        } else {
            info.container_ip.clone()
        }
    }
}

// ========== ContainerLookup trait 实现 ==========

impl shared_types::ContainerLookup for ProjectAdapter {
    /// 根据 user_id 查找容器 IP（ComputerAgentRunner 普通场景）
    ///
    /// user_id 是 1:N（一个 user 可有多个 project），无法用单值索引精确反查。
    /// 此处全量扫描（`find_projects_by_user_id`，已按 `service_type` 过滤）取任一
    /// 匹配 project，再经 `container_info_by_project` 走 `containers[name]` 权威源取 IP——
    /// 同 user 的 Computer 项目共享同一容器，任取一个即可。O(N)，N 为该 user 的 project 数。
    fn find_by_user_id(&self, user_id: &str, service_type: &ServiceType) -> Option<String> {
        // 委托 get_container_by_user_id（同一查找逻辑：扫描 + containers[name] 权威源），
        // 仅取 container_ip。同 user 的 Computer 项目共享同一容器，任取一个即可。
        self.get_container_by_user_id(user_id, service_type)
            .map(|c| self.resolve_backend_addr(&c))
    }

    /// 根据 project_id 查找容器 IP（WebAgentRunner 普通场景）
    ///
    /// 通过 project_to_container 索引找到 container_key，
    /// 然后从 containers 中获取 container_ip。
    ///
    /// 命中容器的 service_type 必须与 `service_type` 一致，否则返回 None。
    fn find_by_project_id(&self, project_id: &str, service_type: &ServiceType) -> Option<String> {
        // clone 出 container_key 后立即释放 project_to_container 读锁
        let container_key = self.project_to_container.get(project_id)?.value().clone();
        let entry = self.containers.get(&container_key)?;
        // 校验 service_type，防止串用
        if entry.service_type() != *service_type {
            debug!(
                "[CONTAINER_LOOKUP] service_type mismatch: expected={:?}, found={:?}, project_id={}",
                service_type,
                entry.service_type(),
                project_id
            );
            return None;
        }
        Some(self.resolve_backend_addr(&entry.info()))
    }

    /// 根据 pod_id 和 service_type 查找容器 IP（共享容器场景）
    ///
    /// 通过 pod_id_to_project_id 索引找到 project_id，
    /// 然后通过 project_to_container 索引找到 container_key，
    /// 最后从 containers 中获取 container_ip。
    ///
    /// 命中容器的 service_type 必须与 `service_type` 一致，否则返回 None，
    /// 避免同一 pod_id 下跨 ServiceType 容器互相串用。
    fn find_by_pod_id(&self, pod_id: &str, service_type: &ServiceType) -> Option<String> {
        // 索引链查找：每步 clone 出 key 后立即释放读锁，避免跨 map 同时持锁
        let project_id = self.pod_id_to_project_id.get(pod_id)?.value().clone();
        let container_key = self.project_to_container.get(&project_id)?.value().clone();
        let entry = self.containers.get(&container_key)?;
        // 校验 service_type，防止串用
        if entry.service_type() != *service_type {
            debug!(
                "[CONTAINER_LOOKUP] service_type mismatch: expected={:?}, found={:?}, pod_id={}",
                service_type,
                entry.service_type(),
                pod_id
            );
            return None;
        }
        Some(self.resolve_backend_addr(&entry.info()))
    }

    /// 按 project_id 反查项目归属 scope（tenant_id/space_id/isolation_type）。
    ///
    /// 直接查 `projects` map（O(1)），不走 container 索引链。命中项目的 service_type
    /// 必须与入参一致（防串用，与 find_by_project_id 同策略）。供 Pingora 注入
    /// `X-Ttyd-Tenant-Id`/`X-Ttyd-Space-Id`，agent_runner 据此解析终端 cwd。
    fn find_project_scope(
        &self,
        project_id: &str,
        service_type: &ServiceType,
    ) -> Option<shared_types::ProjectScope> {
        let info = self.projects.get(project_id)?;
        // 校验 service_type，防止跨 ServiceType 串用
        if info.service_type().as_ref() != Some(service_type) {
            debug!(
                "[CONTAINER_LOOKUP] find_project_scope service_type mismatch: expected={:?}, found={:?}, project_id={}",
                service_type,
                info.service_type(),
                project_id
            );
            return None;
        }
        Some(shared_types::ProjectScope {
            tenant_id: info.tenant_id().map(str::to_string),
            space_id: info.space_id().map(str::to_string),
            isolation_type: info.isolation_type().map(str::to_string),
        })
    }
}
