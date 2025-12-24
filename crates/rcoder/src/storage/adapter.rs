//! 项目适配器
//!
//! 提供统一的项目数据访问接口，替代原有的 3 个 DashMap：
//! - project_and_agent_map: DashMap<String, Arc<ProjectAndContainerInfo>>
//! - sessions: DashMap<String, Arc<ProjectAndContainerInfo>>
//! - session_to_container_id: DashMap<String, String>

use chrono::{DateTime, Utc};
use duckdb_manager::{ContainerRecord, DuckDbStorage, ProjectRecord, StorageStats, UnifiedStorage};
use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo, ServiceType};
use std::sync::Arc;
use tracing::{debug, warn};

use super::bridge::DataBridge;

/// 项目适配器
///
/// 统一管理项目、会话和容器数据，替代原有的 3 个 DashMap
#[derive(Clone)]
pub struct ProjectAdapter {
    storage: Arc<DuckDbStorage>,
}

impl ProjectAdapter {
    /// 创建新的项目适配器
    pub fn new() -> Result<Self, duckdb_manager::DuckDbError> {
        let storage = DuckDbStorage::new()?;
        Ok(Self {
            storage: Arc::new(storage),
        })
    }

    /// 从现有存储创建适配器
    pub fn from_storage(storage: Arc<DuckDbStorage>) -> Self {
        Self { storage }
    }

    // ========== project_and_agent_map 替代方法 ==========

    /// 获取项目信息（替代 project_and_agent_map.get）
    pub fn get(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        match self.storage.get_project(project_id) {
            Ok(Some(record)) => {
                // 获取关联的容器信息
                let container = self.get_container_for_project(&record);
                Some(Arc::new(DataBridge::project_record_to_info(
                    &record, container,
                )))
            }
            Ok(None) => None,
            Err(e) => {
                warn!("获取项目 {} 失败: {}", project_id, e);
                None
            }
        }
    }

    /// 插入或更新项目信息（替代 project_and_agent_map.insert）
    pub fn insert(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
    ) -> Result<(), duckdb_manager::DuckDbError> {
        // 如果有容器信息，先保存容器
        if let Some(container) = info.container() {
            let container_record =
                DataBridge::container_info_to_record(container, info.service_type());
            self.storage.save_container(&container_record)?;
        }

        // 保存项目记录
        let record = DataBridge::info_to_project_record(&info, &project_id);
        self.storage.save_project(&record)?;

        debug!("插入项目: {}", project_id);
        Ok(())
    }

    /// 删除项目（替代 project_and_agent_map.remove）
    pub fn remove(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        // 先获取现有数据
        let info = self.get(project_id)?;

        // 删除项目记录
        if let Err(e) = self.storage.delete_project(project_id) {
            warn!("删除项目 {} 失败: {}", project_id, e);
            return None;
        }

        debug!("删除项目: {}", project_id);
        Some(info)
    }

    /// 检查项目是否存在（替代 project_and_agent_map.contains_key）
    pub fn contains_key(&self, project_id: &str) -> bool {
        match self.storage.project_exists(project_id) {
            Ok(exists) => exists,
            Err(e) => {
                warn!("检查项目 {} 是否存在失败: {}", project_id, e);
                false
            }
        }
    }

    /// 获取所有项目（替代 project_and_agent_map.iter）
    pub fn iter(&self) -> impl Iterator<Item = (String, Arc<ProjectAndContainerInfo>)> + '_ {
        let projects = self.storage.get_all_projects().unwrap_or_default();
        projects.into_iter().filter_map(move |record| {
            let container = self.get_container_for_project(&record);
            let info = DataBridge::project_record_to_info(&record, container);
            Some((record.project_id.clone(), Arc::new(info)))
        })
    }

    /// 获取项目数量（替代 project_and_agent_map.len）
    pub fn len(&self) -> usize {
        match self.storage.get_stats() {
            Ok(stats) => stats.total_projects,
            Err(_) => 0,
        }
    }

    /// 检查是否为空（替代 project_and_agent_map.is_empty）
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // ========== sessions 替代方法 ==========

    /// 通过会话ID获取项目信息（替代 sessions.get）
    pub fn get_by_session_id(&self, session_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        match self.storage.get_project_by_session(session_id) {
            Ok(Some(record)) => {
                let container = self.get_container_for_project(&record);
                Some(Arc::new(DataBridge::project_record_to_info(
                    &record, container,
                )))
            }
            Ok(None) => None,
            Err(e) => {
                warn!("通过会话ID {} 获取项目失败: {}", session_id, e);
                None
            }
        }
    }

    /// 更新会话信息（替代 sessions.insert 和会话更新逻辑）
    pub fn update_session(
        &self,
        project_id: &str,
        session_id: &str,
    ) -> Result<(), duckdb_manager::DuckDbError> {
        self.storage.update_session(project_id, session_id)?;
        debug!(
            "更新会话: project_id={}, session_id={}",
            project_id, session_id
        );
        Ok(())
    }

    // ========== session_to_container_id 替代方法 ==========

    /// 通过会话ID获取容器ID（替代 session_to_container_id.get）
    pub fn get_container_id_by_session(&self, session_id: &str) -> Option<String> {
        match self.storage.get_container_id_by_session(session_id) {
            Ok(container_id) => container_id,
            Err(e) => {
                warn!("通过会话ID {} 获取容器ID失败: {}", session_id, e);
                None
            }
        }
    }

    // ========== 活动时间更新方法 ==========

    /// 更新项目活动时间，返回实际更新使用的时间戳
    pub fn update_activity(&self, project_id: &str) -> Option<DateTime<Utc>> {
        match self.storage.update_project_activity(project_id) {
            Ok(Some(updated_time)) => {
                // 使用相同的时间更新关联容器的活动时间
                if let Ok(Some(record)) = self.storage.get_project(project_id) {
                    let _ = self
                        .storage
                        .update_container_activity_with_time(&record.container_id, updated_time);
                }
                Some(updated_time)
            }
            Ok(None) => None,
            Err(e) => {
                warn!("更新项目 {} 活动时间失败: {}", project_id, e);
                None
            }
        }
    }

    /// 更新会话活动时间
    pub fn update_session_activity(&self, session_id: &str) -> bool {
        match self.storage.update_session_activity(session_id) {
            Ok(updated) => {
                if updated {
                    // 同时更新关联容器的活动时间
                    if let Some(container_id) = self.get_container_id_by_session(session_id) {
                        let _ = self.storage.update_container_activity(&container_id);
                    }
                }
                updated
            }
            Err(e) => {
                warn!("更新会话 {} 活动时间失败: {}", session_id, e);
                false
            }
        }
    }

    // ========== Agent 状态更新方法 ==========

    /// 原子更新 Agent 状态
    pub fn update_agent_status(
        &self,
        project_id: &str,
        status_code: i32,
        status_name: &str,
    ) -> Result<bool, duckdb_manager::DuckDbError> {
        self.storage
            .update_agent_status(project_id, status_code, status_name)
    }

    // ========== 容器相关方法 ==========

    /// 保存容器信息
    pub fn save_container(
        &self,
        container: &ContainerBasicInfo,
        service_type: Option<ServiceType>,
    ) -> Result<(), duckdb_manager::DuckDbError> {
        let record = DataBridge::container_info_to_record(container, service_type);
        self.storage.save_container(&record)
    }

    /// 获取容器信息
    pub fn get_container(&self, container_id: &str) -> Option<ContainerBasicInfo> {
        match self.storage.get_container(container_id) {
            Ok(Some(record)) => Some(DataBridge::container_record_to_info(&record)),
            Ok(None) => None,
            Err(e) => {
                warn!("获取容器 {} 失败: {}", container_id, e);
                None
            }
        }
    }

    /// 删除容器及其关联的项目
    pub fn delete_container_with_projects(
        &self,
        container_id: &str,
    ) -> Result<(bool, usize), duckdb_manager::DuckDbError> {
        self.storage.delete_container_with_projects(container_id)
    }

    /// 按服务类型获取所有容器
    pub fn get_containers_by_service_type(
        &self,
        service_type: ServiceType,
    ) -> Vec<ContainerBasicInfo> {
        match self.storage.get_containers_by_service_type(service_type) {
            Ok(records) => records
                .iter()
                .map(DataBridge::container_record_to_info)
                .collect(),
            Err(e) => {
                warn!("按服务类型获取容器失败: {}", e);
                Vec::new()
            }
        }
    }

    /// 获取所有容器记录
    pub fn get_all_container_records(
        &self,
    ) -> Result<Vec<ContainerRecord>, duckdb_manager::DuckDbError> {
        self.storage.get_all_containers()
    }

    /// 根据容器ID获取关联的项目列表
    pub fn get_projects_by_container_id(
        &self,
        container_id: &str,
    ) -> Result<Vec<ProjectRecord>, duckdb_manager::DuckDbError> {
        self.storage.get_projects_by_container(container_id)
    }

    // ========== ComputerAgentRunner 模式专用方法 ==========

    /// 通过用户ID获取容器信息（ComputerAgentRunner模式）
    ///
    /// 在 ComputerAgentRunner 模式中，一个用户对应一个容器
    /// 这个方法通过 user_id 查找对应的项目记录，然后获取关联的容器信息
    pub fn get_container_by_user_id(&self, user_id: &str) -> Option<ContainerBasicInfo> {
        match self.storage.find_by_user_id(user_id) {
            Ok(Some(project_record)) => {
                // 通过项目记录中的 container_id 获取容器信息
                self.get_container(&project_record.container_id)
            }
            Ok(None) => {
                debug!("未找到用户 {} 的项目记录", user_id);
                None
            }
            Err(e) => {
                warn!("通过用户ID {} 查找项目失败: {}", user_id, e);
                None
            }
        }
    }

    // ========== 清理相关方法 ==========

    /// 查找闲置容器
    pub fn find_idle_containers(
        &self,
        idle_minutes: i64,
        protection_minutes: i64,
    ) -> Vec<duckdb_manager::IdleContainerInfo> {
        match self
            .storage
            .find_idle_containers(idle_minutes, protection_minutes)
        {
            Ok(containers) => containers,
            Err(e) => {
                warn!("查找闲置容器失败: {}", e);
                Vec::new()
            }
        }
    }

    /// 获取存储统计信息
    pub fn get_stats(&self) -> StorageStats {
        match self.storage.get_stats() {
            Ok(stats) => stats,
            Err(e) => {
                warn!("获取存储统计失败: {}", e);
                StorageStats::default()
            }
        }
    }

    // ========== 内部辅助方法 ==========

    /// 获取项目关联的容器信息
    fn get_container_for_project(&self, record: &ProjectRecord) -> Option<ContainerBasicInfo> {
        match self.storage.get_container(&record.container_id) {
            Ok(Some(container)) => Some(DataBridge::container_record_to_info(&container)),
            _ => None,
        }
    }
}

impl Default for ProjectAdapter {
    fn default() -> Self {
        Self::new().expect("创建 ProjectAdapter 失败")
    }
}

impl std::fmt::Debug for ProjectAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectAdapter")
            .field("storage", &"<DuckDbStorage>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_info(project_id: &str) -> ProjectAndContainerInfo {
        let mut info = ProjectAndContainerInfo::new(project_id.to_string());
        info.set_service_type(Some(ServiceType::RCoder));
        info
    }

    #[test]
    fn test_project_crud() {
        let adapter = ProjectAdapter::new().unwrap();

        let project_id = "test-project-1";
        let info = Arc::new(create_test_info(project_id));

        // 插入
        adapter
            .insert(project_id.to_string(), info.clone())
            .unwrap();
        assert!(adapter.contains_key(project_id));

        // 获取
        let retrieved = adapter.get(project_id);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().project_id(), project_id);

        // 删除
        let removed = adapter.remove(project_id);
        assert!(removed.is_some());
        assert!(!adapter.contains_key(project_id));
    }

    #[test]
    fn test_session_operations() {
        let adapter = ProjectAdapter::new().unwrap();

        let project_id = "test-project-2";
        let session_id = "test-session-1";

        // 创建项目
        let mut info = create_test_info(project_id);
        info.set_container(Some(ContainerBasicInfo {
            container_id: "c1".to_string(),
            container_name: "container-1".to_string(),
            container_ip: "127.0.0.1".to_string(),
            internal_port: 8080,
            external_port: 8080,
            project_id: project_id.to_string(),
            status: "running".to_string(),
            created_at: chrono::Utc::now(),
            service_url: "http://localhost:8080".to_string(),
        }));
        adapter
            .insert(project_id.to_string(), Arc::new(info))
            .unwrap();

        // 更新会话
        adapter.update_session(project_id, session_id).unwrap();

        // 通过会话ID获取
        let by_session = adapter.get_by_session_id(session_id);
        assert!(by_session.is_some());
        assert_eq!(by_session.unwrap().project_id(), project_id);

        // 获取容器ID
        let container_id = adapter.get_container_id_by_session(session_id);
        assert_eq!(container_id, Some("c1".to_string()));
    }

    #[test]
    fn test_iter() {
        let adapter = ProjectAdapter::new().unwrap();

        // 插入多个项目
        for i in 0..3 {
            let project_id = format!("iter-project-{}", i);
            let info = Arc::new(create_test_info(&project_id));
            adapter.insert(project_id, info).unwrap();
        }

        // 验证迭代
        let count = adapter.iter().count();
        assert_eq!(count, 3);
    }
}
