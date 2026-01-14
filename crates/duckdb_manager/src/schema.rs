//! DuckDB Schema 定义和初始化
//!
//! 包含数据库表结构定义和初始化逻辑

use crate::connection::DuckDbConnection;
use crate::error::{DuckDbError, DuckDbResult};

/// 容器表 DDL
const CREATE_CONTAINERS_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS containers (
    container_id VARCHAR PRIMARY KEY,
    container_name VARCHAR NOT NULL,
    container_ip VARCHAR NOT NULL,
    internal_port INTEGER NOT NULL,
    external_port INTEGER NOT NULL,
    service_type VARCHAR NOT NULL,
    status VARCHAR NOT NULL,
    service_url VARCHAR NOT NULL,
    created_at TIMESTAMP NOT NULL,
    last_activity TIMESTAMP NOT NULL
)
"#;

/// 项目表 DDL
const CREATE_PROJECTS_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
    project_id VARCHAR PRIMARY KEY,
    session_id VARCHAR,
    service_type VARCHAR NOT NULL,
    container_id VARCHAR NOT NULL,
    user_id VARCHAR,
    agent_status_code INTEGER,
    agent_status_name VARCHAR,
    request_id VARCHAR,
    model_provider_json VARCHAR,
    created_at TIMESTAMP NOT NULL,
    last_activity TIMESTAMP NOT NULL,
    session_created_at TIMESTAMP,
    session_last_activity TIMESTAMP
)
"#;

/// 容器表索引
const CREATE_CONTAINERS_INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS idx_containers_service_type ON containers(service_type)",
    "CREATE INDEX IF NOT EXISTS idx_containers_status ON containers(status)",
    "CREATE INDEX IF NOT EXISTS idx_containers_last_activity ON containers(last_activity)",
];

/// 项目表索引
const CREATE_PROJECTS_INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS idx_projects_session_id ON projects(session_id)",
    "CREATE INDEX IF NOT EXISTS idx_projects_container_id ON projects(container_id)",
    "CREATE INDEX IF NOT EXISTS idx_projects_user_id ON projects(user_id)",
    "CREATE INDEX IF NOT EXISTS idx_projects_service_type ON projects(service_type)",
    "CREATE INDEX IF NOT EXISTS idx_projects_last_activity ON projects(last_activity)",
];

/// Schema 初始化器
pub struct SchemaInitializer;

impl SchemaInitializer {
    /// 初始化数据库 Schema
    ///
    /// 创建所有必需的表和索引
    pub fn initialize(conn: &DuckDbConnection) -> DuckDbResult<()> {
        conn.with_connection(|c| {
            // 创建表
            c.execute(CREATE_CONTAINERS_TABLE, []).map_err(|e| {
                DuckDbError::InitializationError(format!("创建 containers 表失败: {}", e))
            })?;

            c.execute(CREATE_PROJECTS_TABLE, []).map_err(|e| {
                DuckDbError::InitializationError(format!("创建 projects 表失败: {}", e))
            })?;

            // 创建容器表索引
            for sql in CREATE_CONTAINERS_INDEXES {
                c.execute(sql, []).map_err(|e| {
                    DuckDbError::InitializationError(format!("创建容器表索引失败: {}", e))
                })?;
            }

            // 创建项目表索引
            for sql in CREATE_PROJECTS_INDEXES {
                c.execute(sql, []).map_err(|e| {
                    DuckDbError::InitializationError(format!("创建项目表索引失败: {}", e))
                })?;
            }

            tracing::info!("DuckDB Schema 初始化完成");
            Ok(())
        })
    }

    /// 重置数据库（仅用于测试）
    #[cfg(test)]
    pub fn reset(conn: &DuckDbConnection) -> DuckDbResult<()> {
        conn.with_connection(|c| {
            c.execute("DROP TABLE IF EXISTS projects", [])?;
            c.execute("DROP TABLE IF EXISTS containers", [])?;
            Ok(())
        })?;

        Self::initialize(conn)
    }

    /// 验证 Schema 是否正确初始化
    pub fn verify(conn: &DuckDbConnection) -> DuckDbResult<bool> {
        conn.with_connection(|c| {
            // 检查 containers 表
            let containers_exists: i32 = {
                let mut stmt = c.prepare(
                    "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'containers'"
                )?;
                let mut rows = stmt.query([])?;
                let row = rows
                    .next()?
                    .ok_or_else(|| DuckDbError::InternalError("无法查询表信息".to_string()))?;
                row.get(0)?
            };

            // 检查 projects 表
            let projects_exists: i32 = {
                let mut stmt = c.prepare(
                    "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'projects'",
                )?;
                let mut rows = stmt.query([])?;
                let row = rows
                    .next()?
                    .ok_or_else(|| DuckDbError::InternalError("无法查询表信息".to_string()))?;
                row.get(0)?
            };

            Ok(containers_exists > 0 && projects_exists > 0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initialize_schema() {
        let conn = DuckDbConnection::open_in_memory().unwrap();
        let result = SchemaInitializer::initialize(&conn);
        assert!(result.is_ok());
    }

    #[test]
    fn test_verify_schema() {
        let conn = DuckDbConnection::open_in_memory().unwrap();
        SchemaInitializer::initialize(&conn).unwrap();

        let verified = SchemaInitializer::verify(&conn).unwrap();
        assert!(verified);
    }

    #[test]
    fn test_reset_schema() {
        let conn = DuckDbConnection::open_in_memory().unwrap();
        SchemaInitializer::initialize(&conn).unwrap();

        // 插入一些数据
        conn.with_connection(|c| {
            c.execute(
                "INSERT INTO containers (container_id, container_name, container_ip, internal_port, external_port, service_type, status, service_url, created_at, last_activity) VALUES ('c1', 'name1', '127.0.0.1', 8080, 8080, 'rcoder', 'running', 'http://localhost', NOW(), NOW())",
                [],
            )?;
            Ok(())
        }).unwrap();

        // 重置
        SchemaInitializer::reset(&conn).unwrap();

        // 验证数据已清空
        let count: i32 = conn
            .with_connection(|c| {
                let mut stmt = c.prepare("SELECT COUNT(*) FROM containers")?;
                let mut rows = stmt.query([])?;
                let row = rows.next()?.unwrap();
                Ok(row.get(0)?)
            })
            .unwrap();

        assert_eq!(count, 0);
    }

    #[test]
    fn test_idempotent_initialization() {
        let conn = DuckDbConnection::open_in_memory().unwrap();

        // 多次初始化应该是幂等的
        SchemaInitializer::initialize(&conn).unwrap();
        SchemaInitializer::initialize(&conn).unwrap();
        SchemaInitializer::initialize(&conn).unwrap();

        let verified = SchemaInitializer::verify(&conn).unwrap();
        assert!(verified);
    }
}
