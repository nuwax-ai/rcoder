//! 存储适配层
//!
//! 提供从 DashMap 到 DuckDB 存储的适配器

mod adapter;
mod bridge;

pub use adapter::ProjectAdapter;
pub use bridge::DataBridge;
