//! 容器查找服务（统一数据源）
//!
//! 提供容器查找的核心逻辑，避免 Pingora 代理层自己维护容器映射。
//! 使用 DashMap 实现无锁并发访问。

use dashmap::DashMap;
use std::sync::Arc;
use tracing::debug;

use crate::{ProjectAndContainerInfo, ServiceType};
use crate::container_entry::ContainerEntry;

/// 容器查找接口（trait）
///
/// 统一数据源，避免 Pingora 代理层自己维护容器映射。
/// ProjectAdapter 和 ContainerLookupService 都实现这个 trait。
pub trait ContainerLookup: Send + Sync {
    /// 根据 user_id 查找容器 IP（ComputerAgentRunner 普通场景）
    fn find_by_user_id(&self, user_id: &str) -> Option<String>;

    /// 根据 project_id 查找容器 IP（WebAgentRunner 普通场景）
    fn find_by_project_id(&self, project_id: &str) -> Option<String>;

    /// 根据 pod_id 和 service_type 查找容器 IP（共享容器场景）
    fn find_by_pod_id(&self, pod_id: &str, service_type: &ServiceType) -> Option<String>;

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
                    let result = self.find_by_user_id(uid);
                    if result.is_some() {
                        debug!(
                            "[CONTAINER_LOOKUP] Found by user_id: user_id={}",
                            uid
                        );
                    }
                    result
                } else {
                    None
                }
            }
            ServiceType::WebAgentRunner => {
                if let Some(pid) = project_id {
                    let result = self.find_by_project_id(pid);
                    if result.is_some() {
                        debug!(
                            "[CONTAINER_LOOKUP] Found by project_id: project_id={}",
                            pid
                        );
                    }
                    result
                } else {
                    None
                }
            }
        }
    }
}

/// 容器查找服务（统一数据源）
///
/// 提供容器查找的核心逻辑，避免 Pingora 代理层自己维护容器映射。
/// 使用 DashMap 实现无锁并发访问。
pub struct ContainerLookupService {
    /// project_id → project 信息（主存储）
    projects: DashMap<String, Arc<ProjectAndContainerInfo>>,
    /// container_key → 容器条目（带引用计数，Arc 共享确保原子状态一致）
    containers: DashMap<String, Arc<ContainerEntry>>,
    /// project_id → container_key（反向索引）
    project_to_container: DashMap<String, String>,
    /// user_id → project_id（按用户 ID 快速查找）
    user_id_to_project_id: DashMap<String, String>,
    /// pod_id → project_id（按 pod ID 快速查找）
    pod_id_to_project_id: DashMap<String, String>,
}

impl ContainerLookupService {
    /// 创建新的容器查找服务
    pub fn new() -> Self {
        Self {
            projects: DashMap::new(),
            containers: DashMap::new(),
            project_to_container: DashMap::new(),
            user_id_to_project_id: DashMap::new(),
            pod_id_to_project_id: DashMap::new(),
        }
    }

    /// 根据 user_id 查找容器 IP（ComputerAgentRunner 普通场景）
    pub fn find_by_user_id(&self, user_id: &str) -> Option<String> {
        let project_id = self.user_id_to_project_id.get(user_id)?;
        let container_key = self.project_to_container.get(project_id.value())?;
        self.containers.get(container_key.value())
            .map(|entry| entry.container_ip())
    }

    /// 根据 project_id 查找容器 IP（WebAgentRunner 普通场景）
    pub fn find_by_project_id(&self, project_id: &str) -> Option<String> {
        let container_key = self.project_to_container.get(project_id)?;
        self.containers.get(container_key.value())
            .map(|entry| entry.container_ip())
    }

    /// 根据 pod_id 和 service_type 查找容器 IP（共享容器场景）
    pub fn find_by_pod_id(&self, pod_id: &str, _service_type: &ServiceType) -> Option<String> {
        let project_id = self.pod_id_to_project_id.get(pod_id)?;
        let container_key = self.project_to_container.get(project_id.value())?;
        self.containers.get(container_key.value())
            .map(|entry| entry.container_ip())
    }

    /// 查找容器 IP（统一入口）
    ///
    /// 优先级：pod_id > user_id/project_id
    pub fn find_container_ip(
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
                    let result = self.find_by_user_id(uid);
                    if result.is_some() {
                        debug!(
                            "[CONTAINER_LOOKUP] Found by user_id: user_id={}",
                            uid
                        );
                    }
                    result
                } else {
                    None
                }
            }
            ServiceType::WebAgentRunner => {
                if let Some(pid) = project_id {
                    let result = self.find_by_project_id(pid);
                    if result.is_some() {
                        debug!(
                            "[CONTAINER_LOOKUP] Found by project_id: project_id={}",
                            pid
                        );
                    }
                    result
                } else {
                    None
                }
            }
        }
    }

    // ========== Project CRUD ==========

    /// 获取项目信息
    pub fn get_project(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        self.projects.get(project_id).map(|r| r.value().clone())
    }

    /// 插入或更新项目信息
    pub fn insert_project(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
    ) {
        // 更新反向索引
        let container_key = info.container_key().to_string();
        self.project_to_container.insert(project_id.clone(), container_key.clone());

        // 更新 user_id 索引
        if let Some(uid) = info.user_id() {
            self.user_id_to_project_id.insert(uid.to_string(), project_id.clone());
        }

        // 更新 pod_id 索引
        if let Some(pid) = info.pod_id() {
            self.pod_id_to_project_id.insert(pid.to_string(), project_id.clone());
        }

        // 如果有容器信息，插入到 containers DashMap
        if let Some(container) = info.container() {
            let service_type = info.service_type().unwrap_or(ServiceType::WebAgentRunner);
            let entry = Arc::new(ContainerEntry::new(container.clone(), service_type));
            self.containers.insert(container_key, entry);
        }

        self.projects.insert(project_id, info);
    }

    /// 删除项目
    pub fn remove_project(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        if let Some((_, info)) = self.projects.remove(project_id) {
            // 清理反向索引
            self.project_to_container.remove(project_id);

            // 清理 user_id 索引
            if let Some(uid) = info.user_id() {
                self.user_id_to_project_id.remove(uid);
            }

            // 清理 pod_id 索引
            if let Some(pid) = info.pod_id() {
                self.pod_id_to_project_id.remove(pid);
            }

            Some(info)
        } else {
            None
        }
    }

    // ========== Container CRUD ==========

    /// 获取容器条目
    pub fn get_container(&self, container_key: &str) -> Option<Arc<ContainerEntry>> {
        self.containers.get(container_key).map(|r| r.value().clone())
    }

    /// 插入容器条目
    pub fn insert_container(
        &self,
        container_key: String,
        entry: Arc<ContainerEntry>,
    ) {
        self.containers.insert(container_key, entry);
    }

    /// 删除容器条目
    pub fn remove_container(&self, container_key: &str) -> Option<Arc<ContainerEntry>> {
        self.containers.remove(container_key).map(|(_, v)| v)
    }

    /// 检查容器是否存在
    pub fn has_container(&self, container_key: &str) -> bool {
        self.containers.contains_key(container_key)
    }

    // ========== 统计信息 ==========

    /// 获取容器数量
    pub fn container_count(&self) -> usize {
        self.containers.len()
    }

    /// 获取项目数量
    pub fn project_count(&self) -> usize {
        self.projects.len()
    }

    /// 列出所有容器键
    pub fn list_container_keys(&self) -> Vec<String> {
        self.containers.iter().map(|r| r.key().clone()).collect()
    }

    /// 列出所有项目 ID
    pub fn list_project_ids(&self) -> Vec<String> {
        self.projects.iter().map(|r| r.key().clone()).collect()
    }
}

impl Default for ContainerLookupService {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for ContainerLookupService {
    fn clone(&self) -> Self {
        Self {
            projects: self.projects.iter().map(|r| (r.key().clone(), r.value().clone())).collect(),
            containers: self.containers.iter().map(|r| (r.key().clone(), r.value().clone())).collect(),
            project_to_container: self.project_to_container.iter().map(|r| (r.key().clone(), r.value().clone())).collect(),
            user_id_to_project_id: self.user_id_to_project_id.iter().map(|r| (r.key().clone(), r.value().clone())).collect(),
            pod_id_to_project_id: self.pod_id_to_project_id.iter().map(|r| (r.key().clone(), r.value().clone())).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use crate::ContainerBasicInfo;

    fn make_container_info(container_id: &str, container_ip: &str) -> ContainerBasicInfo {
        ContainerBasicInfo {
            container_id: container_id.to_string(),
            container_name: format!("test-{}", container_id),
            container_ip: container_ip.to_string(),
            internal_port: 8086,
            external_port: 0,
            project_id: "proj-1".to_string(),
            status: "running".to_string(),
            created_at: Utc::now(),
            service_url: "http://test".to_string(),
        }
    }

    #[test]
    fn test_find_by_user_id() {
        let service = ContainerLookupService::new();

        // 创建项目信息
        let mut info = ProjectAndContainerInfo::new("proj-1".to_string());
        info.set_user_id(Some("user-1".to_string()));
        info.set_container(Some(make_container_info("container-1", "10.0.0.1")));
        let info = Arc::new(info);

        service.insert_project("proj-1".to_string(), info);

        // 查找
        let result = service.find_by_user_id("user-1");
        assert_eq!(result, Some("10.0.0.1".to_string()));
    }

    #[test]
    fn test_find_by_project_id() {
        let service = ContainerLookupService::new();

        // 创建项目信息
        let mut info = ProjectAndContainerInfo::new("proj-1".to_string());
        info.set_container(Some(make_container_info("container-1", "10.0.0.1")));
        let info = Arc::new(info);

        service.insert_project("proj-1".to_string(), info);

        // 查找
        let result = service.find_by_project_id("proj-1");
        assert_eq!(result, Some("10.0.0.1".to_string()));
    }

    #[test]
    fn test_find_by_pod_id() {
        let service = ContainerLookupService::new();

        // 创建项目信息
        let mut info = ProjectAndContainerInfo::new("proj-1".to_string());
        info.set_pod_id(Some("pod-1".to_string()));
        info.set_container(Some(make_container_info("container-1", "10.0.0.1")));
        let info = Arc::new(info);

        service.insert_project("proj-1".to_string(), info);

        // 查找
        let result = service.find_by_pod_id("pod-1", &ServiceType::ComputerAgentRunner);
        assert_eq!(result, Some("10.0.0.1".to_string()));
    }

    #[test]
    fn test_find_container_ip() {
        let service = ContainerLookupService::new();

        // 创建项目信息
        let mut info = ProjectAndContainerInfo::new("proj-1".to_string());
        info.set_user_id(Some("user-1".to_string()));
        info.set_container(Some(make_container_info("container-1", "10.0.0.1")));
        let info = Arc::new(info);

        service.insert_project("proj-1".to_string(), info);

        // 查找（ComputerAgentRunner 场景）
        let result = service.find_container_ip(
            &ServiceType::ComputerAgentRunner,
            Some("user-1"),
            None,
            None,
        );
        assert_eq!(result, Some("10.0.0.1".to_string()));

        // 查找（WebAgentRunner 场景）
        let result = service.find_container_ip(
            &ServiceType::WebAgentRunner,
            None,
            Some("proj-1"),
            None,
        );
        assert_eq!(result, Some("10.0.0.1".to_string()));
    }

    #[test]
    fn test_not_found() {
        let service = ContainerLookupService::new();

        let result = service.find_by_user_id("nonexistent");
        assert_eq!(result, None);

        let result = service.find_by_project_id("nonexistent");
        assert_eq!(result, None);
    }

    #[test]
    fn test_remove_project() {
        let service = ContainerLookupService::new();

        // 创建项目信息
        let mut info = ProjectAndContainerInfo::new("proj-1".to_string());
        info.set_user_id(Some("user-1".to_string()));
        info.set_container(Some(make_container_info("container-1", "10.0.0.1")));
        let info = Arc::new(info);

        service.insert_project("proj-1".to_string(), info);

        // 删除项目
        service.remove_project("proj-1");

        // 验证已删除
        let result = service.find_by_user_id("user-1");
        assert_eq!(result, None);

        let result = service.find_by_project_id("proj-1");
        assert_eq!(result, None);
    }
}
