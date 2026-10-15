//! 容器域类型 —— 运行态容器契约（条目 / 查找 / 清理 / 统计 + 服务与隔离枚举）
//!
//! 本模块收拢散落在 crate 根目录的容器域文件，实现高内聚：
//! - [`ContainerEntry`]：容器条目（refcount + 活跃时间跟踪，RAII 清理的源头）
//! - [`ContainerLookup`] / [`ProjectScope`]：容器 IP 反查契约（Pingora 代理层数据源）
//! - [`CleanupRequest`]：refcount 归零触发的 RAII 清理请求（生产端 rcoder-storage，消费端 ResourceReaper）
//! - [`StorageStats`] / [`IdleContainerInfo`]：容器存储统计 / 空闲容器清理
//! - [`ServiceType`]：服务类型枚举（决定容器镜像与标识，域内其余文件的公共依赖）
//! - [`IsolationType`]：容器隔离粒度（tenant / space / project）
//!
//! 对外统一经 crate 根部 re-export 暴露（如 `shared_types::ContainerEntry`），
//! 本模块为 crate 内部组织结构，下游不应依赖 `shared_types::container::` 路径。

pub mod cleanup;
pub mod entry;
pub mod isolation_type;
pub mod lookup;
pub mod service_type;
pub mod storage_stats;

pub use cleanup::{CLEANUP_CHANNEL_CAPACITY, CleanupRequest};
pub use entry::ContainerEntry;
pub use isolation_type::{IsolationType, IsolationTypeError};
pub use lookup::{AppRuntimeIpResolver, ContainerLookup, ProjectScope};
pub use service_type::{
    MissingIdentifier, ServiceType, ServiceTypeError, get_enabled_service_types,
    get_supported_service_types,
};
pub use storage_stats::{IdleContainerInfo, StorageStats};
