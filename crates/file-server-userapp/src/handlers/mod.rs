//! userApp 域 HTTP 边界层（自 file-server 迁出）。
//!
//! handler 负责提取器、DTO、utoipa 注解；路由注册在 [`crate::routes`]。

pub mod static_files;
pub mod userapp;
pub mod userapp_app_files;
pub mod userapp_dev;
pub mod userapp_dev_server;
pub mod userapp_files;
