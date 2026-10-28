//! 存储层装配（存储实现已迁至 rcoder-storage crate）
//!
//! 自 M1 起 project/session/container 的存储实现（内存 + 后续 PG）整体迁至
//! `rcoder-storage` crate，本模块只保留 rcoder 运行时侧组件：
//! - 注册表对账；物理计算、连接和路由退休由各受控运行时入口完成。
//! - 转发 re-export：调用方（router/cleanup_task/lib）路径不变，零改动兼容。
//!
//! 历史职责（纯 DashMap 内存存储 + RAII）见 rcoder-storage crate 文档。

mod registry_reconcile;

pub use rcoder_storage::{
    ContainerEntry, IdleContainerInfo, ProjectAdapter, ProjectStoreBackend, StorageStats,
};
pub(crate) use registry_reconcile::start_registry_reconcile;
pub use shared_types::ProjectStore;
