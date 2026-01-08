//! 统一存储接口
//!
//! 提供 UnifiedStorage trait 和实现，用于抽象数据访问层

use crate::connection::DuckDbConnection;
use crate::error::DuckDbResult;
use crate::models::{
    CleanupResult, ContainerRecord, IdleContainerInfo, ProjectRecord, StorageStats,
};
use crate::repositories::{ContainerRepository, ProjectRepository};
use crate::schema::SchemaInitializer;
use shared_types::ServiceType;
use std::sync::Arc;

/// 统一存储接口
///
/// 提供高层次的数据访问抽象，隐藏底层实现细节
pub trait UnifiedStorage: Send + Sync {
    // ========== 容器操作 ==========

    /// 保存或更新容器
    fn save_container(&self, record: &ContainerRecord) -> DuckDbResult<()>;

    /// 获取容器
    fn get_container(&self, container_id: &str) -> DuckDbResult<Option<ContainerRecord>>;

    /// 删除容器
    fn delete_container(&self, container_id: &str) -> DuckDbResult<bool>;

    /// 检查容器是否存在
    fn container_exists(&self, container_id: &str) -> DuckDbResult<bool>;

    /// 更新容器活动时间
    fn update_container_activity(&self, container_id: &str) -> DuckDbResult<bool>;

    /// 使用指定时间更新容器活动时间（用于保持项目和容器时间一致）
    fn update_container_activity_with_time(
        &self,
        container_id: &str,
        time: chrono::DateTime<chrono::Utc>,
    ) -> DuckDbResult<bool>;

    /// 获取所有容器
    fn get_all_containers(&self) -> DuckDbResult<Vec<ContainerRecord>>;

    /// 按服务类型获取容器
    fn get_containers_by_service_type(
        &self,
        service_type: ServiceType,
    ) -> DuckDbResult<Vec<ContainerRecord>>;

    /// 查找闲置容器
    fn find_idle_containers(
        &self,
        idle_minutes: i64,
        protection_minutes: i64,
    ) -> DuckDbResult<Vec<IdleContainerInfo>>;

    // ========== 项目操作 ==========

    /// 保存或更新项目
    fn save_project(&self, record: &ProjectRecord) -> DuckDbResult<()>;

    /// 获取项目
    fn get_project(&self, project_id: &str) -> DuckDbResult<Option<ProjectRecord>>;

    /// 删除项目
    fn delete_project(&self, project_id: &str) -> DuckDbResult<bool>;

    /// 检查项目是否存在
    fn project_exists(&self, project_id: &str) -> DuckDbResult<bool>;

    /// 更新项目活动时间，返回实际更新使用的时间戳
    fn update_project_activity(
        &self,
        project_id: &str,
    ) -> DuckDbResult<Option<chrono::DateTime<chrono::Utc>>>;

    /// 获取所有项目
    fn get_all_projects(&self) -> DuckDbResult<Vec<ProjectRecord>>;

    /// 根据用户ID查找所有项目（ComputerAgentRunner模式）
    ///
    /// 返回该用户的所有项目记录，按最后活动时间倒序排列
    fn find_projects_by_user_id(&self, user_id: &str) -> DuckDbResult<Vec<ProjectRecord>>;

    /// 获取用户最新活跃项目的容器ID（ComputerAgentRunner核心用例）
    ///
    /// 在 ComputerAgentRunner 模式下，一个用户对应一个容器，
    /// 此方法返回该用户最近活跃项目关联的容器ID
    fn get_latest_container_id_by_user_id(&self, user_id: &str) -> DuckDbResult<Option<String>>;

    // ========== 会话操作 ==========

    /// 根据会话ID获取项目
    fn get_project_by_session(&self, session_id: &str) -> DuckDbResult<Option<ProjectRecord>>;

    /// 根据会话ID获取容器名称
    ///
    /// 与 `get_container_id_by_session` 不同，返回稳定的 `container_name`。
    /// 即使容器被重建，container_name 保持不变，可用于通过 Docker API 查询容器状态。
    fn get_container_name_by_session(&self, session_id: &str) -> DuckDbResult<Option<String>>;

    /// 更新会话
    fn update_session(&self, project_id: &str, session_id: &str) -> DuckDbResult<bool>;

    /// 更新会话活动时间
    fn update_session_activity(&self, session_id: &str) -> DuckDbResult<bool>;

    // ========== 状态操作 ==========

    /// 原子更新 Agent 状态
    fn update_agent_status(
        &self,
        project_id: &str,
        status_code: i32,
        status_name: &str,
    ) -> DuckDbResult<bool>;

    // ========== 关联操作 ==========

    /// 根据容器ID获取关联的项目
    fn get_projects_by_container(&self, container_id: &str) -> DuckDbResult<Vec<ProjectRecord>>;

    /// 删除容器及其关联的项目
    fn delete_container_with_projects(&self, container_id: &str) -> DuckDbResult<(bool, usize)>;

    // ========== 清理操作 ==========

    /// 执行清理（删除闲置的容器和项目）
    fn cleanup(&self, idle_minutes: i64, protection_minutes: i64) -> DuckDbResult<CleanupResult>;

    // ========== 统计操作 ==========

    /// 获取存储统计信息
    fn get_stats(&self) -> DuckDbResult<StorageStats>;
}

/// DuckDB 统一存储实现
pub struct DuckDbStorage {
    conn: DuckDbConnection,
}

impl DuckDbStorage {
    /// 创建新的 DuckDB 存储
    pub fn new() -> DuckDbResult<Self> {
        let conn = DuckDbConnection::open_in_memory()?;
        SchemaInitializer::initialize(&conn)?;

        Ok(Self { conn })
    }

    /// 从现有连接创建存储
    pub fn from_connection(conn: DuckDbConnection) -> DuckDbResult<Self> {
        SchemaInitializer::initialize(&conn)?;
        Ok(Self { conn })
    }

    /// 获取容器 Repository
    ///
    /// 注意：返回的 Repository 共享同一个 `Arc<Mutex<Connection>>`，
    /// 确保并发访问时的线程安全。
    fn containers(&self) -> DuckDbResult<ContainerRepository> {
        Ok(ContainerRepository::new(self.conn.clone()))
    }

    /// 获取项目 Repository
    ///
    /// 注意：返回的 Repository 共享同一个 `Arc<Mutex<Connection>>`，
    /// 确保并发访问时的线程安全。
    fn projects(&self) -> DuckDbResult<ProjectRepository> {
        Ok(ProjectRepository::new(self.conn.clone()))
    }

    /// 执行原始 SQL 查询（仅用于调试）
    ///
    /// ⚠️ 此方法仅用于开发和调试目的
    pub fn execute_raw_query(
        &self,
        sql: &str,
    ) -> DuckDbResult<(Vec<String>, Vec<serde_json::Value>)> {
        self.conn.with_connection(|c| {
            let mut stmt = c.prepare(sql)?;
            let column_count = stmt.column_count();

            // 获取列名
            let columns: Vec<String> = (0..column_count)
                .map(|i| {
                    stmt.column_name(i)
                        .map_or("?".to_string(), |v| v.to_string())
                })
                .collect();

            // 执行查询并收集结果
            let mut rows: Vec<serde_json::Value> = Vec::new();
            let mut result_rows = stmt.query([])?;

            while let Some(row) = result_rows.next()? {
                let mut row_obj = serde_json::Map::new();
                for (i, col_name) in columns.iter().enumerate() {
                    // 尝试以字符串形式获取值
                    let value: serde_json::Value = if let Ok(v) = row.get::<_, String>(i) {
                        serde_json::Value::String(v)
                    } else if let Ok(v) = row.get::<_, i64>(i) {
                        serde_json::Value::Number(v.into())
                    } else if let Ok(v) = row.get::<_, f64>(i) {
                        if let Some(n) = serde_json::Number::from_f64(v) {
                            serde_json::Value::Number(n)
                        } else {
                            serde_json::Value::Null
                        }
                    } else if let Ok(v) = row.get::<_, bool>(i) {
                        serde_json::Value::Bool(v)
                    } else {
                        serde_json::Value::Null
                    };
                    row_obj.insert(col_name.clone(), value);
                }
                rows.push(serde_json::Value::Object(row_obj));
            }

            Ok((columns, rows))
        })
    }
}

impl UnifiedStorage for DuckDbStorage {
    // ========== 容器操作 ==========

    fn save_container(&self, record: &ContainerRecord) -> DuckDbResult<()> {
        self.containers()?.upsert(record)
    }

    fn get_container(&self, container_id: &str) -> DuckDbResult<Option<ContainerRecord>> {
        self.containers()?.find_by_id(container_id)
    }

    fn delete_container(&self, container_id: &str) -> DuckDbResult<bool> {
        self.containers()?.delete(container_id)
    }

    fn container_exists(&self, container_id: &str) -> DuckDbResult<bool> {
        self.containers()?.exists(container_id)
    }

    fn update_container_activity(&self, container_id: &str) -> DuckDbResult<bool> {
        self.containers()?.update_activity(container_id)
    }

    fn update_container_activity_with_time(
        &self,
        container_id: &str,
        time: chrono::DateTime<chrono::Utc>,
    ) -> DuckDbResult<bool> {
        self.containers()?
            .update_activity_with_time(container_id, time)
    }

    fn get_all_containers(&self) -> DuckDbResult<Vec<ContainerRecord>> {
        self.containers()?.find_all()
    }

    fn get_containers_by_service_type(
        &self,
        service_type: ServiceType,
    ) -> DuckDbResult<Vec<ContainerRecord>> {
        self.containers()?.find_by_service_type(service_type)
    }

    fn find_idle_containers(
        &self,
        idle_minutes: i64,
        protection_minutes: i64,
    ) -> DuckDbResult<Vec<IdleContainerInfo>> {
        let mut idle_containers = self
            .containers()?
            .find_idle_containers(idle_minutes, protection_minutes)?;

        // 为每个闲置容器填充关联的项目ID
        let projects_repo = self.projects()?;
        for container in &mut idle_containers {
            let projects = projects_repo.find_by_container_id(&container.container_id)?;
            container.project_ids = projects.iter().map(|p| p.project_id.clone()).collect();
        }

        Ok(idle_containers)
    }

    // ========== 项目操作 ==========

    fn save_project(&self, record: &ProjectRecord) -> DuckDbResult<()> {
        self.projects()?.upsert(record)
    }

    fn get_project(&self, project_id: &str) -> DuckDbResult<Option<ProjectRecord>> {
        self.projects()?.find_by_id(project_id)
    }

    fn delete_project(&self, project_id: &str) -> DuckDbResult<bool> {
        self.projects()?.delete(project_id)
    }

    fn project_exists(&self, project_id: &str) -> DuckDbResult<bool> {
        self.projects()?.exists(project_id)
    }

    fn update_project_activity(
        &self,
        project_id: &str,
    ) -> DuckDbResult<Option<chrono::DateTime<chrono::Utc>>> {
        self.projects()?.update_activity(project_id)
    }

    fn get_all_projects(&self) -> DuckDbResult<Vec<ProjectRecord>> {
        self.projects()?.find_all()
    }

    fn find_projects_by_user_id(&self, user_id: &str) -> DuckDbResult<Vec<ProjectRecord>> {
        self.projects()?.find_projects_by_user_id(user_id)
    }

    fn get_latest_container_id_by_user_id(&self, user_id: &str) -> DuckDbResult<Option<String>> {
        self.projects()?.get_latest_container_id_by_user_id(user_id)
    }

    // ========== 会话操作 ==========

    fn get_project_by_session(&self, session_id: &str) -> DuckDbResult<Option<ProjectRecord>> {
        self.projects()?.find_by_session_id(session_id)
    }

    fn get_container_name_by_session(&self, session_id: &str) -> DuckDbResult<Option<String>> {
        self.projects()?.get_container_name_by_session(session_id)
    }

    fn update_session(&self, project_id: &str, session_id: &str) -> DuckDbResult<bool> {
        self.projects()?.update_session(project_id, session_id)
    }

    /// 更新会话活动时间（同时也更新关联容器的活动时间）
    fn update_session_activity(&self, session_id: &str) -> DuckDbResult<bool> {
        let updated = self.projects()?.update_session_activity(session_id)?;

        // 如果会话活跃，同时也更新关联容器的活跃状态
        if updated {
            // 使用新方法直接通过 session_id 更新容器活动时间，无需获取 container_id
            let _ = self.containers()?.update_activity_by_session(session_id);
        }

        Ok(updated)
    }

    // ========== 状态操作 ==========

    fn update_agent_status(
        &self,
        project_id: &str,
        status_code: i32,
        status_name: &str,
    ) -> DuckDbResult<bool> {
        self.projects()?
            .update_status_atomic(project_id, status_code, status_name)
    }

    // ========== 关联操作 ==========

    fn get_projects_by_container(&self, container_id: &str) -> DuckDbResult<Vec<ProjectRecord>> {
        self.projects()?.find_by_container_id(container_id)
    }

    fn delete_container_with_projects(&self, container_id: &str) -> DuckDbResult<(bool, usize)> {
        // 先删除关联的项目
        let deleted_projects = self.projects()?.delete_by_container_id(container_id)?;

        // 再删除容器
        let container_deleted = self.containers()?.delete(container_id)?;

        Ok((container_deleted, deleted_projects))
    }

    // ========== 清理操作 ==========

    fn cleanup(&self, idle_minutes: i64, protection_minutes: i64) -> DuckDbResult<CleanupResult> {
        let mut result = CleanupResult::new();

        // 查找闲置容器
        let idle_containers = self.find_idle_containers(idle_minutes, protection_minutes)?;

        for container in idle_containers {
            // 删除容器及其关联的项目
            match self.delete_container_with_projects(&container.container_id) {
                Ok((container_deleted, projects_deleted)) => {
                    if container_deleted {
                        result.cleaned_containers += 1;
                    }
                    result.cleaned_projects += projects_deleted;
                }
                Err(e) => {
                    result.add_error(format!("删除容器 {} 失败: {}", container.container_id, e));
                }
            }
        }

        Ok(result)
    }

    // ========== 统计操作 ==========

    fn get_stats(&self) -> DuckDbResult<StorageStats> {
        let containers = self.containers()?;
        let projects = self.projects()?;

        let total_containers = containers.count()?;
        let total_projects = projects.count()?;
        let active_sessions = projects.count_active_sessions()?;
        let projects_by_service_type = projects.count_by_service_type()?;

        // 计算活跃和闲置容器
        let all_containers = containers.find_all()?;
        let idle_threshold_minutes = 30;

        let mut active_containers = 0;
        let mut idle_containers = 0;

        for container in &all_containers {
            if container.is_idle(idle_threshold_minutes) {
                idle_containers += 1;
            } else {
                active_containers += 1;
            }
        }

        Ok(StorageStats {
            total_containers,
            total_projects,
            active_sessions,
            active_containers,
            idle_containers,
            projects_by_service_type,
        })
    }
}

impl Clone for DuckDbStorage {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
        }
    }
}

impl std::fmt::Debug for DuckDbStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DuckDbStorage").finish()
    }
}

/// 创建共享的 UnifiedStorage 实例
pub fn create_storage() -> DuckDbResult<Arc<dyn UnifiedStorage>> {
    Ok(Arc::new(DuckDbStorage::new()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_storage() -> DuckDbStorage {
        DuckDbStorage::new().unwrap()
    }

    #[test]
    fn test_container_crud() {
        let storage = create_test_storage();

        let record = ContainerRecord::new(
            "c1".to_string(),
            "container-1".to_string(),
            "127.0.0.1".to_string(),
            8080,
            8080,
            ServiceType::RCoder,
            "running".to_string(),
            "http://localhost:8080".to_string(),
        );

        // Create
        storage.save_container(&record).unwrap();

        // Read
        let found = storage.get_container("c1").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().container_name, "container-1");

        // Update
        storage.update_container_activity("c1").unwrap();

        // Delete
        storage.delete_container("c1").unwrap();
        assert!(!storage.container_exists("c1").unwrap());
    }

    #[test]
    fn test_project_crud() {
        let storage = create_test_storage();

        let record = ProjectRecord::new("p1".to_string(), ServiceType::RCoder, "c1".to_string());

        // Create
        storage.save_project(&record).unwrap();

        // Read
        let found = storage.get_project("p1").unwrap();
        assert!(found.is_some());

        // Update
        storage.update_project_activity("p1").unwrap();

        // Delete
        storage.delete_project("p1").unwrap();
        assert!(!storage.project_exists("p1").unwrap());
    }

    #[test]
    fn test_session_operations() {
        let storage = create_test_storage();

        // 创建容器 (必须先创建容器,因为项目关联到容器)
        let container = ContainerRecord::new(
            "c1".to_string(),
            "container-1".to_string(),
            "127.0.0.1".to_string(),
            8080,
            8080,
            ServiceType::RCoder,
            "running".to_string(),
            "http://localhost:8080".to_string(),
        );
        storage.save_container(&container).unwrap();

        // 创建项目
        let record = ProjectRecord::new("p1".to_string(), ServiceType::RCoder, "c1".to_string());
        storage.save_project(&record).unwrap();

        // 更新会话
        storage.update_session("p1", "session-1").unwrap();

        // 通过会话ID查询
        let project = storage.get_project_by_session("session-1").unwrap();
        assert!(project.is_some());
        assert_eq!(project.unwrap().project_id, "p1");

        // 获取容器名称
        let container_name = storage.get_container_name_by_session("session-1").unwrap();
        assert_eq!(container_name, Some("container-1".to_string()));
    }

    #[test]
    fn test_delete_container_with_projects() {
        let storage = create_test_storage();

        // 创建容器
        let container = ContainerRecord::new(
            "c1".to_string(),
            "container-1".to_string(),
            "127.0.0.1".to_string(),
            8080,
            8080,
            ServiceType::RCoder,
            "running".to_string(),
            "http://localhost:8080".to_string(),
        );
        storage.save_container(&container).unwrap();

        // 创建多个关联项目
        for i in 1..=3 {
            let project =
                ProjectRecord::new(format!("p{}", i), ServiceType::RCoder, "c1".to_string());
            storage.save_project(&project).unwrap();
        }

        // 删除容器及关联项目
        let (container_deleted, projects_deleted) =
            storage.delete_container_with_projects("c1").unwrap();

        assert!(container_deleted);
        assert_eq!(projects_deleted, 3);

        // 验证数据已删除
        assert!(!storage.container_exists("c1").unwrap());
        assert!(!storage.project_exists("p1").unwrap());
    }

    #[test]
    fn test_get_stats() {
        let storage = create_test_storage();

        // 添加容器
        let container = ContainerRecord::new(
            "c1".to_string(),
            "container-1".to_string(),
            "127.0.0.1".to_string(),
            8080,
            8080,
            ServiceType::RCoder,
            "running".to_string(),
            "http://localhost:8080".to_string(),
        );
        storage.save_container(&container).unwrap();

        // 添加项目
        let project = ProjectRecord::new("p1".to_string(), ServiceType::RCoder, "c1".to_string());
        storage.save_project(&project).unwrap();

        // 添加会话
        storage.update_session("p1", "session-1").unwrap();

        let stats = storage.get_stats().unwrap();

        assert_eq!(stats.total_containers, 1);
        assert_eq!(stats.total_projects, 1);
        assert_eq!(stats.active_sessions, 1);
        assert_eq!(stats.active_containers, 1);
        assert_eq!(stats.idle_containers, 0);
    }
}
