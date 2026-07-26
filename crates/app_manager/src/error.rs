//! app_manager service 层错误类型。
//!
//! 从 models.rs 抽出——错误类型与数据模型职责不同（SRP）：service 抛出强类型错误，
//! handler 用 `impl From<AppOperationError> for AppError` 精确映射 HTTP。
//! models.rs 经 `pub use crate::error::{AppOperationError, AppResult};` re-export，
//! 故 `super::models::*` 与 crate 根 `app_manager::AppOperationError` 均可达（零调用方改动）。

use std::fmt;

use shared_types::error_codes::{
    ERR_APP_ALREADY_EXISTS, ERR_APP_NOT_FOUND, ERR_BACKEND_ERROR, ERR_CONFLICT, ERR_FILE_NOT_FOUND,
    ERR_INVALID_STATE, ERR_VALIDATION,
};

/// app 操作级错误（携带业务错误码，供 handler 精确映射 HTTP）。
///
/// 每个错误场景一个 variant，`code()`/`message()` 用 match 实现，编译器强制穷举
/// （新增 variant 时所有 match 编译报错，OCP）。message 含完整因果链，由 service
/// 层在构造时拼入。handler 通过 `impl From<AppOperationError> for AppError` 直接转换，
/// 无需 downcast / 字符串匹配。
#[derive(Debug)]
pub enum AppOperationError {
    /// 应用不存在（404 ERR_APP_NOT_FOUND）
    NotFound(String),
    /// 应用已存在（409 ERR_APP_ALREADY_EXISTS）
    AlreadyExists(String),
    /// 操作状态非法，如未 delete 就清空存储（409 ERR_INVALID_STATE）
    InvalidState(String),
    /// 文件/目录不存在（404 ERR_FILE_NOT_FOUND）
    FileNotFound(String),
    /// 请求参数校验失败（400 ERR_VALIDATION）
    Validation(String),
    /// 后端运行时错误（500 ERR_BACKEND_ERROR，兜底）
    Backend(String),
    /// 乐观锁冲突（409 ERR_CONFLICT）—— expected_resource_version 不匹配
    Conflict(String),
}

impl AppOperationError {
    /// 业务错误码（ERR_* 常量）
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => ERR_APP_NOT_FOUND,
            Self::AlreadyExists(_) => ERR_APP_ALREADY_EXISTS,
            Self::InvalidState(_) => ERR_INVALID_STATE,
            Self::FileNotFound(_) => ERR_FILE_NOT_FOUND,
            Self::Validation(_) => ERR_VALIDATION,
            Self::Backend(_) => ERR_BACKEND_ERROR,
            Self::Conflict(_) => ERR_CONFLICT,
        }
    }

    /// 人读错误信息（含完整因果链，由 service 构造时拼入）
    pub fn message(&self) -> &str {
        match self {
            Self::NotFound(m)
            | Self::AlreadyExists(m)
            | Self::InvalidState(m)
            | Self::FileNotFound(m)
            | Self::Validation(m)
            | Self::Backend(m)
            | Self::Conflict(m) => m,
        }
    }
}

impl fmt::Display for AppOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code(), self.message())
    }
}

impl std::error::Error for AppOperationError {}

/// app service 操作返回类型
pub type AppResult<T> = std::result::Result<T, AppOperationError>;
