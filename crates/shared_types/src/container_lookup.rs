//! 容器查找接口（trait）
//!
//! 统一数据源，避免 Pingora 代理层自己维护容器映射。
//! `ProjectAdapter` 实现此 trait，提供按 user_id / project_id / pod_id 解析容器 IP 的能力。

use tracing::debug;

use crate::ServiceType;

/// 按 project_id 反查的项目归属 scope（共享容器隔离场景）。
///
/// `tenant_id`/`space_id` 齐全表示项目处于 tenant/space 共享容器，对应三级工作目录
/// `/app/project_workspace/{tenant}/{space}/{project}`；缺失则回退单级
/// `/app/project_workspace/{project}`。`isolation_type` 仅作记录，不参与路径决策。
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectScope {
    /// 租户 ID
    pub tenant_id: Option<String>,
    /// 空间 ID
    pub space_id: Option<String>,
    /// 隔离类型（tenant/space/project）
    pub isolation_type: Option<String>,
}

/// 容器查找接口（trait）
///
/// 统一数据源，避免 Pingora 代理层自己维护容器映射。`ProjectAdapter` 实现此 trait。
///
/// ## service_type 校验
///
/// 所有查找方法都会校验命中容器的 `service_type` 是否与请求一致，
/// 不一致则返回 `None`（跳过该容器）。这能避免同一 user_id / pod_id 下
/// 不同 ServiceType 容器互相串用（例如 user_id=6 同时存在 Computer 和 Web 容器）。
pub trait ContainerLookup: Send + Sync {
    /// 根据 user_id 查找容器 IP（ComputerAgentRunner 普通场景）
    ///
    /// 命中容器的 service_type 必须与 `service_type` 一致，否则返回 None。
    fn find_by_user_id(&self, user_id: &str, service_type: &ServiceType) -> Option<String>;

    /// 根据 project_id 查找容器 IP（WebAgentRunner 普通场景）
    ///
    /// 命中容器的 service_type 必须与 `service_type` 一致，否则返回 None。
    fn find_by_project_id(&self, project_id: &str, service_type: &ServiceType) -> Option<String>;

    /// 根据 pod_id 和 service_type 查找容器 IP（共享容器场景）
    ///
    /// 命中容器的 service_type 必须与 `service_type` 一致，否则返回 None。
    fn find_by_pod_id(&self, pod_id: &str, service_type: &ServiceType) -> Option<String>;

    /// 按 project_id 反查项目归属 scope（tenant_id/space_id/isolation_type）。
    ///
    /// 命中项目的 service_type 必须与 `service_type` 一致，否则返回 None（防串用）。
    /// 供 Pingora 注入 `X-Ttyd-Tenant-Id`/`X-Ttyd-Space-Id`，agent_runner 据此解析终端 cwd。
    /// 反查失败（返回 None）时调用方应安全降级（agent_runner 回退单级路径）。
    fn find_project_scope(
        &self,
        project_id: &str,
        service_type: &ServiceType,
    ) -> Option<ProjectScope>;

    /// 查找容器 IP（统一入口）
    ///
    /// 优先级：pod_id > user_id/project_id
    fn find_container_ip(
        &self,
        service_type: &ServiceType,
        user_id: Option<&str>,
        project_id: Option<&str>,
        pod_id: Option<&str>,
    ) -> Option<String> {
        // 1. 优先使用 pod_id（共享容器场景）
        if let Some(pid) = pod_id {
            let result = self.find_by_pod_id(pid, service_type);
            if result.is_some() {
                debug!(
                    "[CONTAINER_LOOKUP] Found by pod_id: pod_id={}, service_type={:?}",
                    pid, service_type
                );
                return result;
            }
        }

        // 2. 根据 ServiceType 选择路由键
        match service_type {
            ServiceType::ComputerAgentRunner => {
                if let Some(uid) = user_id {
                    let result = self.find_by_user_id(uid, service_type);
                    if result.is_some() {
                        debug!("[CONTAINER_LOOKUP] Found by user_id: user_id={}", uid);
                    }
                    result
                } else {
                    None
                }
            }
            ServiceType::WebAgentRunner => {
                if let Some(pid) = project_id {
                    let result = self.find_by_project_id(pid, service_type);
                    if result.is_some() {
                        debug!("[CONTAINER_LOOKUP] Found by project_id: project_id={}", pid);
                    }
                    result
                } else {
                    None
                }
            }
        }
    }
}
