//! 存储层装配（存储实现已迁至 rcoder-storage crate）
//!
//! 自 M1 起 project/session/container 的存储实现（内存 + 后续 PG）整体迁至
//! `rcoder-storage` crate，本模块只保留 rcoder 运行时侧组件：
//! - `resource_reaper`: ResourceReaper（消费存储层 CleanupRequest，物理销毁容器
//!   + 清理 gRPC 池/SSE 流/Pingora backend；依赖 grpc/pingora/docker_manager，属运行时编排）
//! - 转发 re-export：调用方（router/cleanup_task/lib）路径不变，零改动兼容。
//!
//! 历史职责（纯 DashMap 内存存储 + RAII）见 rcoder-storage crate 文档。

mod resource_reaper;

pub use rcoder_storage::{
    CLEANUP_CHANNEL_CAPACITY, CleanupRequest, ContainerEntry, IdleContainerInfo, ProjectAdapter,
    ProjectStoreBackend, StorageStats,
};
pub use resource_reaper::ResourceReaper;
pub use shared_types::ProjectStore;
