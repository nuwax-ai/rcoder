//! HTTP 边界层。
//!
//! handler 按业务域组织，负责 Axum extractor、请求/响应 DTO、参数校验入口及
//! utoipa 注解。URL 与 method 的统一注册位于 [`crate::routes`]，业务实现位于
//! [`crate::service`]。

pub(crate) mod build;
pub(crate) mod build_support;
pub mod computer;
pub(crate) mod git;
pub(crate) mod health;
pub mod multipart;
pub(crate) mod project;
pub mod static_files;
