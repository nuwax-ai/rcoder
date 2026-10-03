//! 控制面域：CLI 参数、双 CLI 监督入口、owner 分派与只读就绪查询。
pub mod config;
pub(crate) mod managed_owner;
pub mod owner_dispatch;
pub mod readiness_query;
pub mod supervision;
