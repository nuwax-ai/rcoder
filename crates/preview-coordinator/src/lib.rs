//! Custom Page（WebAgentRunner 开发阶段 Vite 预览）协调器。
//!
//! 组件：
//! - [`InProcessPreviewStore`]：进程内权威存储（Compose 单节点/测试；契约断言
//!   与 PG 实现共享 `contract_suite`）。
//! - [`PreviewCoordinator`]：协调服务对象（`shared_types::PreviewCoordination` 实现）——
//!   受理/执行/停止/keep-alive 兼容层/路由解析缓存/后台任务。
//! - [`internal_router`]：跨 Pod 内部执行端点（令牌鉴权，rcoder 主 API 挂载）。
//!
//! 依赖方向约束：本 crate 只依赖 shared_types 与基础库——执行器
//! （file-server DevServerManager）与 PG 后端（rcoder-storage）均以 trait 注入，
//! 由 rcoder 主进程装配（与 WorkspaceResolver 同一注入模式）。npm/Electron
//! 发布链（file-server/file-server-proxy）不依赖本 crate，零 PG 耦合
//! （spec 发布隔离不变量）。
mod activity;
mod cache;
mod config;
pub mod contract_suite;
mod dispatch;
mod evidence;
mod identity;
mod in_process;
mod internal_http;
mod service;
mod tasks;
mod token;

pub use activity::ActivityAccumulator;
pub use cache::RouteCache;
pub use config::CoordinatorConfig;
pub use dispatch::{RemoteDispatch, RemoteDispatchError};
pub use evidence::{HostEvidence, SingleInstanceEvidence, pod_uid_of};
pub use identity::{boot_id, local_host_identity, reboot_reconcile_args};
pub use in_process::InProcessPreviewStore;
pub use internal_http::{INTERNAL_TOKEN_HEADER, internal_router};
pub use service::PreviewCoordinator;
pub use token::{internal_token_from_env, internal_token_from_env_named};

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn in_process_store_satisfies_contract() {
        contract_suite::run(&InProcessPreviewStore::new()).await;
    }
}
