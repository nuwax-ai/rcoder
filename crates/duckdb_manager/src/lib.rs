//! DuckDB Manager
//!
//! 提供基于 DuckDB 内存数据库的存储管理，用于替代 DashMap
//!
//! # 主要特性
//!
//! - **内存模式**: 使用 DuckDB 内存数据库，无需持久化
//! - **两表设计**: `containers` 和 `projects` 表（会话信息已合并到 projects）
//! - **线程安全**: 使用 `Arc<Mutex<Connection>>` 实现线程安全访问
//! - **事务支持**: 对需要原子性的操作（如状态更新）提供事务支持
//!
//! # 示例
//!
//! ```rust,ignore
//! use duckdb_manager::{DuckDbManager, create_storage};
//! use duckdb_manager::models::{ContainerRecord, ProjectRecord};
//! use shared_types::ServiceType;
//!
//! // 创建存储
//! let storage = create_storage().unwrap();
//!
//! // 保存容器
//! let container = ContainerRecord::new(
//!     "c1".to_string(),
//!     "container-1".to_string(),
//!     "127.0.0.1".to_string(),
//!     8080,
//!     8080,
//!     ServiceType::RCoder,
//!     "running".to_string(),
//!     "http://localhost:8080".to_string(),
//! );
//! storage.save_container(&container).unwrap();
//!
//! // 保存项目
//! let project = ProjectRecord::new(
//!     "p1".to_string(),
//!     ServiceType::RCoder,
//!     "c1".to_string(),
//! );
//! storage.save_project(&project).unwrap();
//!
//! // 更新会话
//! storage.update_session("p1", "session-1").unwrap();
//!
//! // 通过会话ID查询
//! let project = storage.get_project_by_session("session-1").unwrap();
//! ```
//!
//! # 模块结构
//!
//! - `connection`: 数据库连接管理
//! - `error`: 错误类型定义
//! - `models`: 数据模型定义
//! - `schema`: 数据库 Schema 定义和初始化
//! - `repositories`: 数据访问层（Repository 模式）
//! - `manager`: 全局管理器
//! - `storage`: 统一存储接口

pub mod connection;
pub mod error;
pub mod manager;
pub mod models;
pub mod repositories;
pub mod schema;
pub mod storage;

// 重新导出常用类型
pub use connection::DuckDbConnection;
pub use error::{DuckDbError, DuckDbResult};
pub use manager::{DuckDbManager, get_global_manager, init_global_manager};
pub use models::{
    CleanupResult, ContainerRecord, IdleContainerInfo, OrphanContainerInfo, ProjectRecord,
    StorageStats,
};
pub use repositories::{ContainerRepository, ProjectRepository};
pub use schema::SchemaInitializer;
pub use storage::{DuckDbStorage, UnifiedStorage, create_storage};
