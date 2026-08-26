//! rcoder 存储层 crate：project/session/container 映射的内存实现 + 可选 PG 持久化后端
//!
//! 自 rcoder/src/storage 迁出（M1）。提供：
//! - O(1) 热路径访问（DashMap 分片内存镜像）
//! - 引用计数容器管理（共享容器安全）
//! - RAII 清理请求（CleanupRequest 经 mpsc 交给 rcoder 侧 ResourceReaper）
//! - `pg` feature：PostgreSQL write-behind 持久化 + 启动全量加载（Phase 1 M3）
//!
//! 数据契约（CleanupRequest / StorageStats / IdleContainerInfo / ProjectStore）统一定义在
//! shared_types（跨 crate 契约单一事实源），本 crate 仅转发导出。
//!
//! 模块组织：
//! - adapter: ProjectAdapter 内存实现（project/session CRUD + ContainerLookup）；
//!   实现细节按职责拆为子模块 adapter/{container_ops,lookup,session_ops,store_impl}.rs
//!   （同类型 extension-impl），tests.rs 为单元测试
//! - backend: ProjectStoreBackend 枚举（静态分发）
//! - config: PostgresConfig（rcoder config.yml `[storage.postgres]` 数据模型）
//! - pg（cfg feature="pg"）: PgStore + writer + 启动加载 + write-behind op 模型
//!   （persist_ops）——feature 门控的代码全部收在本子树

mod adapter;
mod backend;
pub mod config;

#[cfg(feature = "pg")]
pub mod pg;

pub use adapter::ProjectAdapter;
pub use backend::ProjectStoreBackend;
pub use shared_types::{
    CLEANUP_CHANNEL_CAPACITY, CleanupRequest, ContainerEntry, IdleContainerInfo, StorageStats,
};
