//! Axum 主路由装配（目录化拆分）。
//!
//! [`create_router`] 只做「构建各域 → merge → 挂中间件」的装配骨架：
//! - 域路由分属 [`api`]（chat/agent 会话）、[`computer`]（Computer Agent Runner
//!   与 pod 管理）、[`userapp_proxy`]（Pingora 状态 + userApp 工具族文档接口）、
//!   [`devcomputer`]（调试委托）、[`agent_mgmt`]（agent 安装管理）
//! - app_manager / userapp_forward / file-server-admin 为外部 crate 或
//!   crate 级模块的路由，装配在本文件
//! - 全局中间件与安全头在 [`layers`]（层序敏感）；/metrics 在 [`metrics`]
//!
//! // AppState/SessionInfo 拆至 app_state.rs（状态+会话注册表自成一档）；
//! // re-export 保持 crate::router::AppState 既有引用稳定。

// router 整体由 binary (main.rs) 使用，lib 内不直接调用 create_router / ApiDoc 等。
// 抑制 dead_code 以避免 lib 维度误报。
#![allow(dead_code)]

mod agent_mgmt;
mod api;
mod computer;
mod devcomputer;
mod layers;
mod metrics;
mod userapp_proxy;

use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};

use crate::handler;
use rcoder_telemetry::TelemetryGuard;

pub use crate::app_state::AppState;

/// 内部 API 路由（供 rcoder-gateway 调用）
///
/// 这些端点挂载在中间件之后、鉴权层之前 merge，绕过 API Key 鉴权。
fn create_internal_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/internal/pod/ensure", post(handler::internal_pod_ensure))
        .route(
            "/internal/session/{session_id}/resolve",
            get(handler::internal_session_resolve),
        )
        .with_state(state)
}

/// 健康检查路由
fn health_routes(state: &Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(handler::health_check))
        .with_state(state.clone())
}

/// 调试路由（仅用于开发和问题排查，需要 feature flag "debug" 启用）
#[cfg(feature = "debug")]
fn debug_routes(state: &Arc<AppState>) -> Router {
    Router::new()
        .route("/debug/sql", get(handler::debug_dump_summary))
        .route("/debug/projects", get(handler::debug_list_projects))
        .route("/debug/containers", get(handler::debug_list_containers))
        .route("/debug/storage/stats", get(handler::debug_storage_stats))
        .with_state(state.clone())
}

/// app_manager 域（外部 crate；upload 压缩包 1GiB 覆盖全局 50MB 限制）
fn app_manager_routes(state: &Arc<AppState>) -> Router {
    let app_manager_state = Arc::new(app_manager::handlers::AppManagerState {
        app_service: state.app_service.clone(),
        // 共享客户端 (连接超时 + 连接池复用; SSE 流不能设总超时, 见 http_client 模块)
        http_client: crate::http_client::shared_client().clone(),
    });
    app_manager::routes::app_manager_routes()
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024 * 1024))
        .with_state(app_manager_state)
}

/// file-server 基础路由 + computer 域拦截层（构造失败不阻断主服务启动，
/// warn 可见、缺路由面可诊断）。
///
/// TS 移植版老路径：/api/project、/api/computer、/api/git、/api/build、/api/page；
/// 排除 /api/v1/userapp——由 rcoder 转发层接管。与 TS 行为一致不设 API key
/// （merge 在 api-key layer 之后，同 internal 先例）；computer 域拦截层：
/// header X-Service-Type=userapp 的请求短路转发到该 app 开发容器
/// （反向代理转来的 TS 老路径，body 零解析）。
fn file_server_routes_with_intercept(state: &Arc<AppState>) -> Router {
    match crate::file_server_embed::merged_router() {
        Ok(fs_router) => fs_router.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::userapp_forward::computer_intercept,
        )),
        Err(e) => {
            tracing::warn!("file-server routes not mounted on main service: {e}");
            Router::new()
        }
    }
}

/// 创建 Axum 路由
pub fn create_router(state: Arc<AppState>, telemetry: Option<Arc<TelemetryGuard>>) -> Router {
    let mut router = Router::new()
        .merge(health_routes(&state))
        .merge(api::api_routes(state.clone()))
        .merge(computer::computer_routes(state.clone()))
        .merge(devcomputer::devcomputer_routes(state.clone()))
        .merge(userapp_proxy::proxy_api_routes(state.clone()))
        .merge(agent_mgmt::agent_mgmt_routes(state.clone()))
        .merge(app_manager_routes(&state))
        // userApp 文件域转发层: /api/v1/userapp 本地入口 + 容器侧接口显式透传清单
        // （build/tasks/static 等构建链接口在 file-server 侧，逐条登记于
        // CONTAINER_PASS_THROUGH_PATHS；原 {*rest} 通配与 {app_id} 参数路由同树时
        // matchit 冲突启动即 panic，已移除）
        .merge(crate::userapp_forward::routes().with_state(state.clone()))
        // file-server 分流代理运行时启停 (无 state, 受全局 API key 中间件保护;
        // `rcoder file-server {start,stop,restart,status}` CLI 的服务端)
        .merge(crate::file_server_admin::admin_routes());

    // 仅在启用 debug feature 时添加调试路由
    #[cfg(feature = "debug")]
    {
        router = router.merge(debug_routes(&state));
    }

    // /metrics 端点（仅启用 Prometheus 时）
    if let Some(ref guard) = telemetry {
        router = metrics::mount(router, guard);
    }

    // 🆕 克隆共享的 API Key 配置用于中间件
    let api_key_config = Arc::clone(&state.api_key_config);

    // 全局中间件 → internal / file-server 两面在鉴权层之后 merge（不受
    // API Key 约束的既有语义）→ 安全响应头覆盖全部面。
    layers::apply_security_headers(
        layers::apply_global_middleware(router, api_key_config)
            // 内部 API（供 rcoder-gateway 调用，绕过 API Key 鉴权）
            .merge(create_internal_routes(state.clone()))
            .merge(file_server_routes_with_intercept(&state)),
    )
}
