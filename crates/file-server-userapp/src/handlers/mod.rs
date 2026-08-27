//! userApp 域 HTTP 边界层（自 file-server 迁出）。
//!
//! handler 负责提取器与 utoipa 注解；wire 契约类型在 [`crate::models`]，
//! 跨 crate 共享实现在 `file_server::ops`；路由注册在 [`crate::routes`]。

pub mod static_files;
pub mod userapp;
pub mod userapp_app_files;
pub mod userapp_dev;
pub mod userapp_dev_server;
pub mod userapp_files;
