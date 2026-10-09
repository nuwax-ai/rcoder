//! 代理配置：pingap 配置生成（用 pingap-config 官方类型）。

pub mod admin_probe;
pub mod apply_status;
pub mod compiler;
pub(crate) mod config_source;
pub mod pingap;

pub(crate) mod publication;
#[cfg(test)]
mod publication_tests;
