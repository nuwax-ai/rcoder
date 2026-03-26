//! DuckDB 全局管理器
//!
//! 提供 DuckDB 存储的全局管理入口

use crate::connection::DuckDbConnection;
use crate::error::DuckDbResult;
use crate::models::StorageStats;
use crate::repositories::{ContainerRepository, ProjectRepository};
use crate::schema::SchemaInitializer;
use parking_lot::RwLock;
use std::sync::Arc;

/// DuckDB 全局管理器
///
/// 管理数据库连接和提供存储访问入口
pub struct DuckDbManager {
    /// 数据库连接
    conn: DuckDbConnection,
    /// 是否已初始化
    initialized: Arc<RwLock<bool>>,
}

impl DuckDbManager {
    /// 创建新的 DuckDB 管理器（内存模式）
    pub fn new_in_memory() -> DuckDbResult<Self> {
        let conn = DuckDbConnection::open_in_memory()?;

        let manager = Self {
            conn,
            initialized: Arc::new(RwLock::new(false)),
        };

        // 初始化 Schema
        manager.initialize()?;

        Ok(manager)
    }

    /// 初始化数据库 Schema
    fn initialize(&self) -> DuckDbResult<()> {
        let mut initialized = self.initialized.write();

        if *initialized {
            return Ok(());
        }

        SchemaInitializer::initialize(&self.conn)?;
        *initialized = true;

        tracing::info!("DuckDB Manager 初始化完成");
        Ok(())
    }

    /// 获取容器 Repository
    ///
    /// 注意：返回的 Repository 共享同一个 `Arc<Mutex<Connection>>`，
    /// 确保并发访问时的线程安全。
    pub fn containers(&self) -> DuckDbResult<ContainerRepository> {
        Ok(ContainerRepository::new(self.conn.clone()))
    }

    /// 获取项目 Repository
    ///
    /// 注意：返回的 Repository 共享同一个 `Arc<Mutex<Connection>>`，
    /// 确保并发访问时的线程安全。
    pub fn projects(&self) -> DuckDbResult<ProjectRepository> {
        Ok(ProjectRepository::new(self.conn.clone()))
    }

    /// 获取存储统计信息
    pub fn get_stats(&self) -> DuckDbResult<StorageStats> {
        let containers = self.containers()?;
        let projects = self.projects()?;

        let total_containers = containers.count()?;
        let total_projects = projects.count()?;
        let active_sessions = projects.count_active_sessions()?;
        let projects_by_service_type = projects.count_by_service_type()?;

        // 计算活跃和闲置容器
        let all_containers = containers.find_all()?;
        let idle_threshold_minutes = 30; // 30 分钟闲置阈值

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

    /// 验证数据库状态
    pub fn verify(&self) -> DuckDbResult<bool> {
        SchemaInitializer::verify(&self.conn)
    }

    /// 获取原始连接（用于高级操作）
    pub fn connection(&self) -> &DuckDbConnection {
        &self.conn
    }

    /// 执行自定义 SQL 查询（用于复杂查询）
    pub fn execute_query<F, T>(&self, f: F) -> DuckDbResult<T>
    where
        F: FnOnce(&duckdb::Connection) -> DuckDbResult<T>,
    {
        self.conn.with_connection(f)
    }

    /// 执行事务
    pub fn transaction<F, T>(&self, f: F) -> DuckDbResult<T>
    where
        F: FnOnce(&duckdb::Connection) -> DuckDbResult<T>,
    {
        self.conn.transaction(f)
    }
}

impl Clone for DuckDbManager {
    fn clone(&self) -> Self {
        // 注意：克隆后的 Manager 共享相同的底层连接
        Self {
            conn: self.conn.clone(),
            initialized: self.initialized.clone(),
        }
    }
}

impl std::fmt::Debug for DuckDbManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DuckDbManager")
            .field("initialized", &*self.initialized.read())
            .finish()
    }
}

/// 全局 DuckDB 管理器单例
static GLOBAL_MANAGER: std::sync::OnceLock<DuckDbManager> = std::sync::OnceLock::new();

/// 初始化全局 DuckDB 管理器
///
/// 应在应用启动时调用一次
pub fn init_global_manager() -> DuckDbResult<&'static DuckDbManager> {
    // 尝试初始化，如果已初始化则返回现有实例
    if let Some(manager) = GLOBAL_MANAGER.get() {
        return Ok(manager);
    }

    let manager = DuckDbManager::new_in_memory()?;

    // 尝试设置全局管理器，如果失败（已被其他线程设置）则返回现有实例
    match GLOBAL_MANAGER.set(manager) {
        Ok(()) => Ok(GLOBAL_MANAGER.get().unwrap()),
        Err(_) => Ok(GLOBAL_MANAGER.get().unwrap()),
    }
}

/// 获取全局 DuckDB 管理器
///
/// 如果未初始化，将自动初始化
pub fn get_global_manager() -> DuckDbResult<&'static DuckDbManager> {
    match GLOBAL_MANAGER.get() {
        Some(manager) => Ok(manager),
        None => init_global_manager(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ContainerRecord, ProjectRecord};
    use shared_types::ServiceType;

    #[test]
    fn test_new_in_memory() {
        let manager = DuckDbManager::new_in_memory().unwrap();
        assert!(manager.verify().unwrap());
    }

    #[test]
    fn test_get_stats_empty() {
        let manager = DuckDbManager::new_in_memory().unwrap();
        let stats = manager.get_stats().unwrap();

        assert_eq!(stats.total_containers, 0);
        assert_eq!(stats.total_projects, 0);
        assert_eq!(stats.active_sessions, 0);
    }

    #[test]
    fn test_get_stats_with_data() {
        let manager = DuckDbManager::new_in_memory().unwrap();

        // 添加容器
        let containers = manager.containers().unwrap();
        containers
            .upsert(&ContainerRecord::new(
                "c1".to_string(),
                "container-1".to_string(),
                "127.0.0.1".to_string(),
                8080,
                8080,
                ServiceType::RCoder,
                "running".to_string(),
                "http://localhost:8080".to_string(),
            ))
            .unwrap();

        // 添加项目
        let projects = manager.projects().unwrap();
        projects
            .upsert(&ProjectRecord::new(
                "p1".to_string(),
                ServiceType::RCoder,
                "c1".to_string(),
            ))
            .unwrap();

        // 添加会话
        projects.update_session("p1", "session-1").unwrap();

        let stats = manager.get_stats().unwrap();
        assert_eq!(stats.total_containers, 1);
        assert_eq!(stats.total_projects, 1);
        assert_eq!(stats.active_sessions, 1);
    }

    #[test]
    fn test_clone_shares_data() {
        let manager = DuckDbManager::new_in_memory().unwrap();

        // 使用原始 manager 添加数据
        let containers = manager.containers().unwrap();
        containers
            .upsert(&ContainerRecord::new(
                "c1".to_string(),
                "container-1".to_string(),
                "127.0.0.1".to_string(),
                8080,
                8080,
                ServiceType::RCoder,
                "running".to_string(),
                "http://localhost:8080".to_string(),
            ))
            .unwrap();

        // 克隆 manager
        let cloned = manager.clone();

        // 通过克隆的 manager 读取数据
        let cloned_containers = cloned.containers().unwrap();
        let found = cloned_containers.find_by_id("c1").unwrap();
        assert!(found.is_some());
    }

    #[test]
    fn test_transaction() {
        let manager = DuckDbManager::new_in_memory().unwrap();

        // 成功的事务
        let result = manager.transaction(|conn| {
            conn.execute(
                "INSERT INTO containers (container_id, container_name, container_ip, internal_port, external_port, service_type, status, service_url, created_at, last_activity) VALUES ('c1', 'name', '127.0.0.1', 8080, 8080, 'rcoder', 'running', 'http://localhost', NOW(), NOW())",
                [],
            )?;
            Ok(())
        });
        assert!(result.is_ok());

        // 验证数据存在
        let containers = manager.containers().unwrap();
        assert!(containers.exists("c1").unwrap());
    }
}
