//! file-server-userapp：UserApp 域（`/api/v1/userapp/*`）独立 crate。
//!
//! 自 file-server 拆出（洋葱模型：本 crate 依赖 file-server 共享设施，反向无依赖）：
//! - 错误出口 [`error`]：`UserAppError` 直渲 HttpResult + 语义状态码，
//!   file-server `AppError` 经 `From` 在跨 crate 边界翻译（`?` 直接传播）；
//! - 共享单例（dev_server 进程表 / log_cache / build_manager / config）经
//!   [`UserAppState::fs`] 引用，编译任务表为域内自有；
//! - 组装 API [`full_router`]/[`container_router`] 供 file-server-proxy（npm 唯一
//!   分发链）与 agent_runner（开发容器宿主）消费。
//!
//! TS 对齐路由（/api/project、/api/computer 等）仍在 file-server，不受本 crate 影响。

pub mod error;
pub(crate) mod handlers;
pub mod models;
pub mod routes;
pub mod service;
pub mod state;

use anyhow::Result;
use axum::Router;

pub use error::{UserAppError, reply, success_reply};
pub use routes::document;
pub use state::UserAppState;

/// 独立全量形态：file-server 全量 router（TS 对齐 + swagger + fallback）+
/// `/api/v1/userapp` 子树（同款公共中间件栈）。file-server-proxy embed 消费。
pub fn full_router(server: &file_server::FileServer) -> Result<Router> {
    let base = server.router()?;
    Ok(base.merge(userapp_subrouter(server)?))
}

/// 开发容器形态：file-server container 路由集 + `/api/v1/userapp` 子树。
/// agent_runner embed 消费（容器是 userApp 域本地实现的宿主）。
pub fn container_router(server: &file_server::FileServer) -> Result<Router> {
    let base = server.router_container()?;
    Ok(base.merge(userapp_subrouter(server)?))
}

/// `/api/v1/userapp` 子树（独立 state：UserAppState；公共中间件栈与 file-server
/// 侧同源——`apply_common_layers` 单一事实源）。
fn userapp_subrouter(server: &file_server::FileServer) -> Result<Router> {
    let body_limit = usize::try_from(server.state().config.request_body_max_bytes)
        .map_err(|e| anyhow::anyhow!("REQUEST_BODY_MAX_BYTES exceeds platform usize: {e}"))?;
    let (router, _openapi) = routes::userapp_top_router().split_for_parts();
    let state = UserAppState::new(server.state());
    Ok(file_server::server::apply_common_layers(router, body_limit).with_state(state))
}
