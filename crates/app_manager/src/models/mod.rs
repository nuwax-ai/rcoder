//! 应用管理服务数据模型（facade——按域拆分到子模块）
//!
//! 子模块划分：
//! - [`commons`]：共享类型 + 枚举（请求/响应两侧均引用）
//! - [`request`]：API 请求体
//! - [`response`]：API 响应体
//! - [`storage`]：持久存储管理（v2 §5.4）
//! - [`db`]：数据库管理（app-runtime 自带 PG）

pub mod commons;
pub mod db;
pub mod logs;
pub mod release;
pub mod request;
pub mod response;
pub mod storage;

pub use commons::*;
pub use db::*;
pub use logs::*;
pub use release::*;
pub use request::*;
pub use response::*;
pub use storage::*;

// 错误类型抽到 error.rs（SRP：错误与数据模型分离），此处 re-export 保持
// `super::models::*` 调用方零改动（12 个内部文件均经此 glob 引用 AppOperationError）。
pub use crate::error::{AppOperationError, AppResult};

/// 应用端口运行时状态（来自 container-runtime-api，含实际分配的对外端口）
pub use container_runtime_api::AppPortStatus;

#[cfg(test)]
mod wire_tests;
