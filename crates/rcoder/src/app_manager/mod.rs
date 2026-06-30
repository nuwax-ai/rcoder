//! 应用管理服务模块
//!
//! 提供应用生命周期管理 API，支持 Docker 和 K8s 两种部署模式。

pub mod app_service_trait;
pub mod config;
pub mod handlers;
pub mod k8s_service;
pub mod models;
pub mod routes;
pub mod service;

// 重新导出常用类型
pub use app_service_trait::AppServiceTrait;
pub use config::AppManagerConfig;
pub use models::*;
