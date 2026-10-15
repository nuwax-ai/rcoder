//! app_manager —— Userapp 应用生命周期管理（独立 crate）
//!
//! 提供 REST API 管理 Userapp：create/update/delete/start/stop/restart +
//! 文件管理 + 持久存储 + DB 管理（reset-password/create-database）。
//! Docker / K8s 双后端统一走 ContainerRuntime 抽象。
//! rcoder 无状态：业务元数据由调用方持久化。

pub mod activity_registry;
pub mod app_service_trait;
pub mod config;
pub mod error;
pub mod handlers;
mod lifecycle;
pub mod models;
mod ops;
mod release_flow;
pub mod routes;
mod runtime;
pub mod service;
#[cfg(test)]
mod test_support;
pub mod utils;

pub use activity_registry::AppActivityRegistry;
pub use app_service_trait::AppServiceTrait;
pub use config::{AppAccessMode, AppManagerConfig};
pub use models::*;
