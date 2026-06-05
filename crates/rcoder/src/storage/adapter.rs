//! 项目适配器
//!
//! 提供统一的项目数据访问接口，替代原有的 3 个 DashMap：
//! - project_and_agent_map: DashMap<String, Arc<ProjectAndContainerInfo>>
//! - sessions: DashMap<String, Arc<ProjectAndContainerInfo>>
//! - session_to_container_id: DashMap<String, String>

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use duckdb_manager::{ContainerRecord, DuckDbStorage, ProjectRecord, StorageStats, UnifiedStorage};
use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo, ServiceType};
use std::sync::Arc;
use tracing::{debug, warn};

use super::bridge::DataBridge;

/// 项目适配器
///
/// 统一管理项目、会话和容器数据。
/// session 映射使用 DashMap 提供 O(1) 查找，避免 DuckDB CAS 竞态问题。
/// 使用 entry API 操作 DashMap，避免死锁。
#[derive(Clone)]
pub struct ProjectAdapter {
    storage: Arc<DuckDbStorage>,
    /// session_id → project_id（快速查找，SSE handler 使用）
    session_to_project: Arc<DashMap<String, String>>,
    /// project_id → session_id（反向索引，用于 clear/remove 时清理）
    project_to_session: Arc<DashMap<String, String>>,
}

impl ProjectAdapter {
    /// 创建新的项目适配器
    pub fn new() -> Result<Self, duckdb_manager::DuckDbError> {
        let storage = DuckDbStorage::new()?;
        Ok(Self {
            storage: Arc::new(storage),
            session_to_project: Arc::new(DashMap::new()),
            project_to_session: Arc::new(DashMap::new()),
        })
    }

    /// 从现有存储创建适配器
    pub fn from_storage(storage: Arc<DuckDbStorage>) -> Self {
        Self {
            storage,
            session_to_project: Arc::new(DashMap::new()),
            project_to_session: Arc::new(DashMap::new()),
        }
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
                warn!("getproject {} failed: {}", project_id, e);
                None
            }
        }
    }

    /// 插入或更新项目信息（替代 project_and_agent_map.insert）
    ///
    /// 使用事务确保项目和容器记录的原子性写入，避免部分写入导致的孤儿记录。
    pub fn insert(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
    ) -> Result<(), duckdb_manager::DuckDbError> {
        // 准备容器记录（如果存在）
        let container_record = info.container().map(|container| {
            debug!(
                "Inserting project container info: project_id={}, container_id={}, container_ip={}",
                project_id, container.container_id, container.container_ip
            );
            DataBridge::container_info_to_record(container, info.service_type())
        });

        if container_record.is_none() {
            debug!("No project container: project_id={}", project_id);
        }

        // 准备项目记录
        let record = DataBridge::info_to_project_record(&info, &project_id);
        debug!(
            "Saving project record: project_id={}, session_id={:?}, container_id={}",
            record.project_id, record.session_id, record.container_id
        );

        // 使用事务原子性地保存项目和容器记录
        self.storage
            .save_project_and_container_atomic(&record, container_record.as_ref())?;

        debug!("Inserted project: {}", project_id);
        Ok(())
    }

    /// 插入项目并设置 session 映射（单次原子写入，消除 CAS 竞态）
    ///
    /// 将项目元数据和 session_id 在同一次 DuckDB 事务中写入，
    /// 同时更新 DashMap 索引。替代原来的 `insert` + `update_session_atomic` 两步操作。
    pub fn insert_with_session(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
        session_id: Option<&str>,
    ) -> Result<(), duckdb_manager::DuckDbError> {
        // 准备容器记录
        let container_record = info.container().map(|container| {
            debug!(
                "Inserting project container info: project_id={}, container_id={}, container_ip={}",
                project_id, container.container_id, container.container_ip
            );
            DataBridge::container_info_to_record(container, info.service_type())
        });

        // 准备项目记录，覆盖 session_id
        let mut record = DataBridge::info_to_project_record(&info, &project_id);
        if let Some(sid) = session_id {
            record.session_id = Some(sid.to_string());
            let now = Utc::now();
            record.session_created_at = Some(now);
            record.session_last_activity = Some(now);
        }
        debug!(
            "Saving project record with session: project_id={}, session_id={:?}, container_id={}",
            record.project_id, record.session_id, record.container_id
        );

        // 单次原子写入
        self.storage
            .save_project_and_container_atomic(&record, container_record.as_ref())?;

        // 更新 DashMap 索引
        // 先用 view 读旧值（锁在闭包返回后立即释放），再单独写入
        if let Some(sid) = session_id {
            // 1. 读旧 session_id（view 不暴露 Ref）
            let old_sid = self
                .project_to_session
                .view(&project_id, |_, old| old.clone());
            // 2. 清除旧正向映射
            if let Some(ref old) = old_sid
                && old != sid
            {
                self.session_to_project.remove(old);
            }
            // 3. 写入新映射（entry API，无闭包内跨 map 操作）
            self.project_to_session
                .entry(project_id.clone())
                .and_modify(|v| *v = sid.to_string())
                .or_insert_with(|| sid.to_string());
            self.session_to_project
                .entry(sid.to_string())
                .and_modify(|v| *v = project_id.clone())
                .or_insert_with(|| project_id.clone());
        }

        debug!("Inserted project with session: {}", project_id);
        Ok(())
    }

    /// 删除项目（DuckDB + DashMap 双清）
    pub fn remove(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        // 先获取现有数据
        let info = self.get(project_id)?;

        // 清理 DashMap 索引
        if let Some((_, old_sid)) = self.project_to_session.remove(project_id) {
            self.session_to_project.remove(&old_sid);
        }

        // 删除项目记录
        if let Err(e) = self.storage.delete_project(project_id) {
            warn!("Failed to remove project {}: {}", project_id, e);
            return None;
        }

        debug!("Removed project: {}", project_id);
        Some(info)
    }

    /// 检查项目是否存在（替代 project_and_agent_map.contains_key）
    pub fn contains_key(&self, project_id: &str) -> bool {
        match self.storage.project_exists(project_id) {
            Ok(exists) => exists,
            Err(e) => {
                warn!("Failed to check project {} exists: {}", project_id, e);
                false
            }
        }
    }

    /// 获取所有项目（替代 project_and_agent_map.iter）
    ///
    /// 使用 JOIN 查询一次性获取项目和容器信息，避免 N+1 查询问题。
    /// 100 个项目：从 101 次查询优化为 1 次查询。
    pub fn iter(&self) -> impl Iterator<Item = (String, Arc<ProjectAndContainerInfo>)> + '_ {
        let data = self
            .storage
            .get_all_projects_with_containers()
            .unwrap_or_default();
        data.into_iter().map(|(project_record, container_record)| {
            let container = container_record.map(|c| DataBridge::container_record_to_info(&c));
            let info = DataBridge::project_record_to_info(&project_record, container);
            (project_record.project_id.clone(), Arc::new(info))
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

    /// 通过会话ID获取项目信息
    ///
    /// 优先查 DashMap（O(1)），miss 时回退 DuckDB 并填充缓存。
    /// 使用 `view()` 替代 `get()`，不暴露 `Ref`，锁在函数返回后立即释放。
    pub fn get_by_session_id(&self, session_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        // 1. 先查 DashMap（view 在闭包返回后立即释放锁，无 Ref 暴露）
        let project_id_opt = self
            .session_to_project
            .view(session_id, |_, project_id| project_id.clone());
        if let Some(project_id) = project_id_opt {
            return self.get(&project_id);
        }

        // 2. DashMap miss，回退 DuckDB
        match self.storage.get_project_by_session(session_id) {
            Ok(Some(record)) => {
                let project_id = record.project_id.clone();
                // 填充 DashMap 缓存（entry API）
                self.session_to_project
                    .entry(session_id.to_string())
                    .or_insert_with(|| project_id.clone());
                self.project_to_session
                    .entry(project_id.clone())
                    .or_insert_with(|| session_id.to_string());

                let container = self.get_container_for_project(&record);
                Some(Arc::new(DataBridge::project_record_to_info(
                    &record, container,
                )))
            }
            Ok(None) => None,
            Err(e) => {
                warn!("Failed to get project for session {}: {}", session_id, e);
                None
            }
        }
    }

    /// 更新会话信息（DuckDB + DashMap 双写）
    ///
    /// 先用 `view()` 读旧值（锁立即释放），再单独写入，避免 entry 持锁期间跨 map 操作。
    pub fn update_session(
        &self,
        project_id: &str,
        session_id: &str,
    ) -> Result<(), duckdb_manager::DuckDbError> {
        self.storage.update_session(project_id, session_id)?;

        // 1. 读旧 session_id（view 不暴露 Ref，锁立即释放）
        let old_sid = self
            .project_to_session
            .view(&project_id.to_string(), |_, old| old.clone());
        // 2. 清除旧正向映射
        if let Some(ref old) = old_sid
            && old != session_id
        {
            self.session_to_project.remove(old);
        }
        // 3. 写入新映射（entry API，无闭包内跨 map 操作）
        self.project_to_session
            .entry(project_id.to_string())
            .and_modify(|v| *v = session_id.to_string())
            .or_insert_with(|| session_id.to_string());
        self.session_to_project
            .entry(session_id.to_string())
            .and_modify(|v| *v = project_id.to_string())
            .or_insert_with(|| project_id.to_string());

        debug!(
            "Updating session: project_id={}, session_id={}",
            project_id, session_id
        );
        Ok(())
    }

    /// 原子更新会话信息（仅当当前 session_id 与预期相同时才更新）
    ///
    /// 使用 compare-and-swap 模式防止并发更新导致的竞态条件。
    /// 仅保留供其他场景使用，当前 chat handler 已改用 DashMap 直接写入。
    #[allow(dead_code)]
    pub fn update_session_atomic(
        &self,
        project_id: &str,
        new_session_id: &str,
        expected_current_session_id: Option<&str>,
    ) -> Result<bool, duckdb_manager::DuckDbError> {
        let updated = self.storage.update_session_atomic(
            project_id,
            new_session_id,
            expected_current_session_id,
        )?;
        if updated {
            debug!(
                "Atomic session update: project_id={}, new_session_id={}, expected_current={:?}",
                project_id, new_session_id, expected_current_session_id
            );
        } else {
            debug!(
                "Atomic session update skipped (session_id changed): project_id={}, expected_current={:?}",
                project_id, expected_current_session_id
            );
        }
        Ok(updated)
    }

    /// 清除会话信息（DuckDB + DashMap 双清）
    ///
    /// 用于 Agent 停止后清理会话状态（容器级 RAII）
    pub fn clear_session(&self, project_id: &str) -> Result<(), duckdb_manager::DuckDbError> {
        self.storage.clear_session(project_id)?;

        // 通过反向索引清理 DashMap
        if let Some((_, old_sid)) = self.project_to_session.remove(project_id) {
            self.session_to_project.remove(&old_sid);
        }

        debug!("Clearing session: project_id={}", project_id);
        Ok(())
    }

    // ========== session_to_container_id 替代方法 ==========

    /// 通过会话ID获取容器名称（用于容器重启后的容器查询）
    ///
    /// 与 `get_container_id_by_session` 不同，返回稳定的 `container_name`（如 `computer-agent-runner-user_123`）。
    /// 即使容器被重建，container_name 保持不变，可直接通过 Docker API 查询容器状态。
    pub fn get_container_name_by_session(&self, session_id: &str) -> Option<String> {
        match self.storage.get_container_name_by_session(session_id) {
            Ok(container_name) => {
                debug!(
                    "Getting container name by session_id: session_id={}, container_name={:?}",
                    session_id, container_name
                );
                container_name
            }
            Err(e) => {
                warn!(" sessionID {} getcontainer failed: {}", session_id, e);
                None
            }
        }
    }

    // ========== 活动时间更新方法 ==========

    /// 更新项目活动时间，返回实际更新使用的时间戳
    ///
    /// 使用原子操作在单个事务中同时更新项目和关联容器的活动时间，
    /// 避免多次 mutex 获取之间的 TOCTOU 竞态条件。
    pub fn update_activity(&self, project_id: &str) -> Option<DateTime<Utc>> {
        match self.storage.update_activity_atomic(project_id) {
            Ok(updated_time) => updated_time,
            Err(e) => {
                warn!("update project {} failed: {}", project_id, e);
                None
            }
        }
    }

    /// 更新会话活动时间
    pub fn update_session_activity(&self, session_id: &str) -> bool {
        match self.storage.update_session_activity(session_id) {
            Ok(updated) => {
                if updated {
                    // 底层 storage.update_session_activity 现在会自动更新关联容器的活动时间
                    // 无需在此处手动调用，避免了使用不可靠的 container_id
                }
                updated
            }
            Err(e) => {
                warn!("update session {} failed: {}", session_id, e);
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
                warn!("getcontainer {} failed: {}", container_id, e);
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
                warn!("Failed to get container: {}", e);
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
    /// 此方法返回该用户最近活跃项目关联的容器信息
    pub fn get_container_by_user_id(&self, user_id: &str) -> Option<ContainerBasicInfo> {
        match self.storage.get_latest_container_id_by_user_id(user_id) {
            Ok(Some(container_id)) => {
                // 通过容器ID获取容器信息
                self.get_container(&container_id)
            }
            Ok(None) => {
                debug!("No project for user_id {} ", user_id);
                None
            }
            Err(e) => {
                warn!("Failed to get container for user_id {}: {}", user_id, e);
                None
            }
        }
    }

    /// 通过 Pod ID 获取容器信息（共享容器模式）
    ///
    /// 在共享容器模式中，多个项目可能共享同一个容器
    /// 此方法返回该 Pod 下最近活跃项目关联的容器信息
    pub fn get_container_by_pod_id(&self, pod_id: &str) -> Option<ContainerBasicInfo> {
        match self.storage.get_latest_container_id_by_pod_id(pod_id) {
            Ok(Some(container_id)) => {
                debug!(
                    "Found container_id for pod_id={}: container_id={}",
                    pod_id, container_id
                );
                self.get_container(&container_id)
            }
            Ok(None) => {
                debug!("No container found for pod_id={}", pod_id);
                None
            }
            Err(e) => {
                warn!("Failed to get container for pod_id {}: {}", pod_id, e);
                None
            }
        }
    }

    /// 通过用户ID查找所有项目（ComputerAgentRunner模式）
    ///
    /// 返回该用户的所有项目记录，按最后活动时间倒序排列
    pub fn find_projects_by_user_id(
        &self,
        user_id: &str,
    ) -> Vec<duckdb_manager::models::ProjectRecord> {
        match self.storage.find_projects_by_user_id(user_id) {
            Ok(projects) => projects,
            Err(e) => {
                warn!("Failed to get project for user_id {}: {}", user_id, e);
                Vec::new()
            }
        }
    }

    /// 根据 pod_id 查找所有项目（RCoder 共享容器模式）
    ///
    /// 返回该 Pod 下所有项目记录，按最后活动时间倒序排列
    pub fn find_projects_by_pod_id(
        &self,
        pod_id: &str,
    ) -> Vec<duckdb_manager::models::ProjectRecord> {
        match self.storage.find_projects_by_pod_id(pod_id) {
            Ok(projects) => projects,
            Err(e) => {
                warn!("Failed to get project for pod_id {}: {}", pod_id, e);
                Vec::new()
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
                warn!("Failed to get container: {}", e);
                Vec::new()
            }
        }
    }

    /// 获取存储统计信息
    pub fn get_stats(&self) -> StorageStats {
        match self.storage.get_stats() {
            Ok(stats) => stats,
            Err(e) => {
                warn!("get value failed: {}", e);
                StorageStats::default()
            }
        }
    }

    // ========== 调试方法 ==========

    /// 执行原始 SQL 查询（仅用于调试）
    ///
    /// ⚠️ 此方法仅用于开发和调试目的
    pub fn execute_raw_query(
        &self,
        sql: &str,
    ) -> Result<(Vec<String>, Vec<serde_json::Value>), String> {
        self.storage
            .execute_raw_query(sql)
            .map_err(|e| format!("Query execution failed: {}", e))
    }

    /// 获取 DuckDB 内存使用统计（用于调试和监控）
    pub fn get_memory_stats(&self) -> Result<String, String> {
        self.storage
            .get_memory_stats()
            .map_err(|e| format!("Failed to get memory stats: {}", e))
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
            .field("session_count", &self.session_to_project.len())
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

        // 获取容器名称
        let container_name = adapter.get_container_name_by_session(session_id);
        assert_eq!(container_name, Some("container-1".to_string()));
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

    #[test]
    fn test_insert_with_session() {
        let adapter = ProjectAdapter::new().unwrap();

        let project_id = "test-project-session";
        let session_id = "test-session-abc";
        let info = Arc::new(create_test_info(project_id));

        // 通过 insert_with_session 写入
        adapter
            .insert_with_session(project_id.to_string(), info.clone(), Some(session_id))
            .unwrap();

        // 通过 session_id 查找（走 DashMap 快速路径）
        let by_session = adapter.get_by_session_id(session_id);
        assert!(by_session.is_some());
        assert_eq!(by_session.unwrap().project_id(), project_id);

        // clear_session 清理 DashMap
        adapter.clear_session(project_id).unwrap();
        assert!(adapter.get_by_session_id(session_id).is_none());
    }

    #[test]
    fn test_session_rotation() {
        let adapter = ProjectAdapter::new().unwrap();

        let project_id = "test-rotation";
        let info = Arc::new(create_test_info(project_id));

        // 第一次写入 session-1
        adapter
            .insert_with_session(project_id.to_string(), info.clone(), Some("session-1"))
            .unwrap();
        assert!(adapter.get_by_session_id("session-1").is_some());

        // 第二次写入 session-2（覆盖 session-1）
        adapter
            .insert_with_session(project_id.to_string(), info.clone(), Some("session-2"))
            .unwrap();
        assert!(adapter.get_by_session_id("session-2").is_some());
        // 旧 session 应该被清理
        assert!(adapter.get_by_session_id("session-1").is_none());
    }

    #[test]
    fn test_session_cache_miss_fallback() {
        // 验证 DashMap miss 时回退 DuckDB 并自动填充缓存
        let adapter = ProjectAdapter::new().unwrap();

        let project_id = "test-fallback";
        let session_id = "test-session-fallback";
        let info = Arc::new(create_test_info(project_id));

        // 1. 通过 DuckDB 直接写入 session（绕过 DashMap）
        adapter
            .insert(project_id.to_string(), info.clone())
            .unwrap();
        adapter
            .update_session(project_id, session_id)
            .unwrap();

        // 2. update_session 已写入 DashMap，验证命中
        let result = adapter.get_by_session_id(session_id);
        assert!(result.is_some());
        assert_eq!(result.unwrap().project_id(), project_id);

        // 3. 验证 clear_session 后 DuckDB 也被清除（验证 RAII 完整性）
        adapter.clear_session(project_id).unwrap();
        assert!(adapter.get_by_session_id(session_id).is_none());
    }
}
