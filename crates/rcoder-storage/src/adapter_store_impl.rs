//! ProjectStore 契约实现（从 adapter.rs 拆出，extension-impl）
//!
//! 固有方法签名与 [`shared_types::ProjectStore`] 一一对应，转发即可；
//! 拆分动机：adapter.rs 主文件控制在 ~500 行（仓库拆分约定）。

use std::sync::Arc;

use chrono::{DateTime, Utc};
use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo, ServiceType, StorageStats};

use super::adapter::ProjectAdapter;

impl shared_types::ProjectStore for ProjectAdapter {
    fn get(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        self.get(project_id)
    }

    fn contains_key(&self, project_id: &str) -> bool {
        self.contains_key(project_id)
    }

    fn iter(&self) -> Vec<(String, Arc<ProjectAndContainerInfo>)> {
        self.iter()
    }

    fn get_by_session_id(&self, session_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        self.get_by_session_id(session_id)
    }

    fn get_container_name_by_session(&self, session_id: &str) -> Option<String> {
        self.get_container_name_by_session(session_id)
    }

    fn get_all_container_records(&self) -> Vec<ContainerBasicInfo> {
        self.get_all_container_records()
    }

    fn get_projects_by_container_id(
        &self,
        container_id: &str,
    ) -> Vec<Arc<ProjectAndContainerInfo>> {
        self.get_projects_by_container_id(container_id)
    }

    fn get_container_by_user_id(
        &self,
        user_id: &str,
        service_type: &ServiceType,
    ) -> Option<ContainerBasicInfo> {
        self.get_container_by_user_id(user_id, service_type)
    }

    fn get_container_by_pod_id(&self, pod_id: &str) -> Option<ContainerBasicInfo> {
        self.get_container_by_pod_id(pod_id)
    }

    fn find_projects_by_user_id(
        &self,
        user_id: &str,
        service_type: &ServiceType,
    ) -> Vec<Arc<ProjectAndContainerInfo>> {
        self.find_projects_by_user_id(user_id, service_type)
    }

    fn find_projects_by_pod_id(&self, pod_id: &str) -> Vec<Arc<ProjectAndContainerInfo>> {
        self.find_projects_by_pod_id(pod_id)
    }

    fn get_stats(&self) -> StorageStats {
        self.get_stats()
    }

    fn dump_summary(&self) -> String {
        self.dump_summary()
    }

    fn insert(&self, project_id: String, info: Arc<ProjectAndContainerInfo>) -> anyhow::Result<()> {
        self.insert(project_id, info)
    }

    fn insert_with_session(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        self.insert_with_session(project_id, info, session_id)
    }

    fn add_session_to_project(&self, project_id: &str, session_id: &str) -> bool {
        self.add_session_to_project(project_id, session_id)
    }

    fn remove(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        self.remove(project_id)
    }

    fn clear_session(&self, project_id: &str) {
        self.clear_session(project_id);
    }

    fn clear_session_one(&self, project_id: &str, session_id: &str) -> bool {
        self.clear_session_one(project_id, session_id)
    }

    fn update_activity(&self, project_id: &str) -> Option<DateTime<Utc>> {
        self.update_activity(project_id)
    }

    fn update_session_activity(&self, session_id: &str) -> bool {
        self.update_session_activity(session_id)
    }

    fn update_agent_status(&self, project_id: &str, status: i32, message: &str) -> bool {
        self.update_agent_status(project_id, status, message)
    }

    fn delete_container_with_projects(&self, container_id: &str) -> (bool, usize) {
        self.delete_container_with_projects(container_id)
    }
}
