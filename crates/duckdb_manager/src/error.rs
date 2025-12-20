//! DuckDB Manager 错误类型定义
//!
//! 定义 DuckDB 存储管理器的错误类型

use thiserror::Error;

/// DuckDB 存储管理器错误类型
#[derive(Debug, Error)]
pub enum DuckDbError {
    /// 数据库连接错误
    #[error("数据库连接错误: {0}")]
    ConnectionError(String),

    /// SQL 执行错误
    #[error("SQL 执行错误: {0}")]
    QueryError(String),

    /// 数据未找到
    #[error("数据未找到: {entity} with {key} = {value}")]
    NotFound {
        entity: &'static str,
        key: &'static str,
        value: String,
    },

    /// 数据已存在
    #[error("数据已存在: {entity} with {key} = {value}")]
    AlreadyExists {
        entity: &'static str,
        key: &'static str,
        value: String,
    },

    /// 事务错误
    #[error("事务错误: {0}")]
    TransactionError(String),

    /// 序列化/反序列化错误
    #[error("序列化错误: {0}")]
    SerializationError(String),

    /// 数据完整性错误
    #[error("数据完整性错误: {0}")]
    IntegrityError(String),

    /// 初始化错误
    #[error("初始化错误: {0}")]
    InitializationError(String),

    /// 内部错误
    #[error("内部错误: {0}")]
    InternalError(String),
}

impl From<duckdb::Error> for DuckDbError {
    fn from(err: duckdb::Error) -> Self {
        DuckDbError::QueryError(err.to_string())
    }
}

impl From<serde_json::Error> for DuckDbError {
    fn from(err: serde_json::Error) -> Self {
        DuckDbError::SerializationError(err.to_string())
    }
}

/// DuckDB 操作结果类型
pub type DuckDbResult<T> = Result<T, DuckDbError>;
