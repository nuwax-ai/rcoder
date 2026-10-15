//! HTTP 边界层。
//!
//! handler 按业务域组织，负责 Axum extractor、参数校验入口及 utoipa 注解。
//! wire 契约类型在 [`crate::models`]，跨 crate 共享实现（*_impl）在
//! [`crate::ops`]；URL 与 method 的统一注册位于 [`crate::routes`]，业务实现
//! 位于 [`crate::service`]。本层 crate 私有（跨 crate 走 ops/service/models）。

pub(crate) mod build;
pub(crate) mod build_support;
pub mod computer;
pub(crate) mod git;
pub(crate) mod health;
pub(crate) mod project;
pub mod static_files;
