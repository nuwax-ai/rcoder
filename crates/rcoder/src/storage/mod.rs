//! 存储层：纯 DashMap 内存存储 + RAII 自动资源回收
//!
//! 替代 DuckDB 内存模式，提供：
//! - O(1) 热路径访问（project/session 查找）
//! - 引用计数容器管理（共享容器安全）
//! - RAII 清理（移除 project 时自动销毁无引用的容器）

mod adapter;
mod resource_reaper;
mod types;

pub use adapter::ProjectAdapter;
pub use resource_reaper::{CleanupRequest, ResourceReaper};
pub use shared_types::ContainerEntry;
pub use types::{IdleContainerInfo, StorageStats};
