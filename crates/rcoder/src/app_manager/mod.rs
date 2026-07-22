//! 应用管理服务模块
//!
//! 提供应用生命周期管理 API，Docker / K8s 双后端统一走 `ContainerRuntime` 抽象。
//! rcoder 无状态：业务元数据由调用方（Java）持久化，本模块只管 pod 生命周期 + 实时读。

pub mod app_service_trait;
pub mod config;
pub mod files;
pub mod handlers;
pub mod models;
pub mod routes;
pub mod service;
pub mod storage;
pub mod utils;

// 重新导出常用类型
pub use app_service_trait::AppServiceTrait;
pub use config::{AppAccessMode, AppManagerConfig};
pub use models::*;
