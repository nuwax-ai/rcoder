//! app_manager —— UserApp 应用生命周期管理（独立 crate）
//!
//! 提供 REST API 管理 UserApp：create/update/delete/start/stop/restart +
//! 文件管理 + 持久存储 + DB 管理（reset-password/create-database）。
//! Docker / K8s 双后端统一走 ContainerRuntime 抽象。
//! rcoder 无状态：业务元数据由调用方持久化。

mod activity_persistence_ops;
pub mod activity_registry;
pub mod app_create;
pub mod app_db;
pub(crate) mod app_metadata;
pub mod app_ops;
pub mod app_params;
pub mod app_pingora;
pub mod app_service_trait;
mod app_start;
pub mod app_status;
pub mod app_workspace;
pub mod config;
pub mod error;
pub mod files;
pub mod handlers;
pub mod models;
pub(crate) mod release_runtime;
pub mod release_store;
pub mod releases;
pub mod routes;
mod runtime_identity;
pub mod service;
pub mod storage;
#[cfg(test)]
mod test_support;
pub mod utils;

pub use activity_registry::AppActivityRegistry;
pub use app_service_trait::AppServiceTrait;
pub use config::{AppAccessMode, AppManagerConfig};
pub use models::*;
