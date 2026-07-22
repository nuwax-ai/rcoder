//! app_manager —— UserApp 应用生命周期管理（独立 crate）
//!
//! 提供 REST API 管理 UserApp：create/update/delete/start/stop/restart +
//! 文件管理 + 持久存储 + DB 管理（reset-password/create-database）。
//! Docker / K8s 双后端统一走 ContainerRuntime 抽象。
//! rcoder 无状态：业务元数据由调用方持久化。

pub mod app_service_trait;
pub mod config;
pub mod files;
pub mod handlers;
pub mod models;
pub mod routes;
pub mod service;
pub mod storage;
pub mod utils;

pub use app_service_trait::AppServiceTrait;
pub use config::{AppAccessMode, AppManagerConfig};
pub use models::*;
