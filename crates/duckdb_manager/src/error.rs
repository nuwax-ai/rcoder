//! DuckDB Manager error types
//!
//! Defines error types for DuckDB storage manager

use thiserror::Error;

/// DuckDB storage manager error type
#[derive(Debug, Error)]
pub enum DuckDbError {
    /// Database connection error
    #[error("database connection error: {0}")]
    ConnectionError(String),

    /// SQL execution error
    #[error("SQL execution error: {0}")]
    QueryError(String),

    /// Data not found
    #[error("data not found: {entity} with {key} = {value}")]
    NotFound {
        entity: &'static str,
        key: &'static str,
        value: String,
    },

    /// Data already exists
    #[error("data already exists: {entity} with {key} = {value}")]
    AlreadyExists {
        entity: &'static str,
        key: &'static str,
        value: String,
    },

    /// Transaction error
    #[error("transaction error: {0}")]
    TransactionError(String),

    /// Serialization/deserialization error
    #[error("serialization error: {0}")]
    SerializationError(String),

    /// Data integrity error
    #[error("data integrity error: {0}")]
    IntegrityError(String),

    /// Initialization error
    #[error("initialization error: {0}")]
    InitializationError(String),

    /// Internal error
    #[error("internal error: {0}")]
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
