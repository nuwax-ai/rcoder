//! E2B 兼容门面路由（feature `kubernetes`）。
//!
//! 独立鉴权链：控制面用 e2b API key（`X-API-Key` → PG 哈希校验），
//! 不与 rcoder 服务级 key 混用——对应路径已在 ApiKeyValidator 前缀豁免。
//! 数据面不在此路由：请求带 `E2b-Sandbox-Id` 头时由最外层守卫
//! （middleware/sandbox_guard）短路进流式代理，路径与 rcoder 自有路由
//! 天然解耦（/health、/files 同名冲突即由此消解）。
use std::sync::Arc;

use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};

#[cfg(feature = "kubernetes")]
pub use rcoder_engine::service::sandbox_manager::SandboxManager;
#[cfg(not(feature = "kubernetes"))]
enum SandboxManager {} // 占位：非 kubernetes 构建下的不可达空类型
#[cfg(feature = "kubernetes")]
use crate::handler::sandbox as sbx_handler;

/// 门面依赖容器（http-server 侧的注入边界；kubernetes feature 关闭时恒为空）。
#[derive(Clone, Default)]
pub struct SandboxDeps {
    #[cfg(feature = "kubernetes")]
    pub manager: Option<Arc<SandboxManager>>,
    /// 数据面端口白名单（M1.5）；空 = 不放行任何端口。
    pub allowed_ports: Vec<i32>,
}

impl SandboxDeps {
    pub fn manager(&self) -> Option<Arc<SandboxManager>> {
        #[cfg(feature = "kubernetes")]
        {
            self.manager.clone()
        }
        #[cfg(not(feature = "kubernetes"))]
        {
            None::<Arc<SandboxManager>>
        }
    }
    #[cfg(feature = "kubernetes")]
    pub fn new(manager: Arc<SandboxManager>, allowed_ports: Vec<i32>) -> Self {
        Self {
            manager: Some(manager),
            allowed_ports,
        }
    }
}

/// 控制面 e2b key 鉴权中间件：校验通过注入 `Extension<SandboxApiKeyRecord>`。
#[cfg(feature = "kubernetes")]
pub(crate) async fn e2b_auth(
    State(deps): State<SandboxDeps>,
    mut request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let unauthorized = |message: &str| {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "code": 401, "message": message })),
        )
            .into_response()
    };
    let Some(manager) = deps.manager() else {
        return unauthorized("e2b sandbox 未启用");
    };
    let key = request
        .headers()
        .get("X-API-Key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if key.is_empty() {
        return unauthorized("缺少 X-API-Key");
    }
    match manager.authenticate(key).await {
        Ok(record) => {
            request.extensions_mut().insert(record);
            next.run(request).await
        }
        Err(_) => unauthorized("API key 无效"),
    }
}

#[cfg(feature = "kubernetes")]
pub fn sandbox_routes(deps: SandboxDeps) -> Router {
    Router::new()
        .route("/sandboxes", post(sbx_handler::create_sandbox))
        .route("/v2/sandboxes", post(sbx_handler::create_sandbox))
        .route("/v2/sandboxes", get(sbx_handler::list_sandboxes))
        .route("/sandboxes/{sandbox_id}", get(sbx_handler::get_sandbox))
        .route("/sandboxes/{sandbox_id}", delete(sbx_handler::kill_sandbox))
        .route("/sandboxes/{sandbox_id}/timeout", post(sbx_handler::set_timeout))
        .route("/sandboxes/{sandbox_id}/connect", post(sbx_handler::connect_sandbox))
        .route("/v2/sandboxes/{sandbox_id}/connect", post(sbx_handler::connect_sandbox))
        .route("/templates", get(sbx_handler::list_templates))
        .route("/volumes", post(sbx_handler::create_volume))
        .route("/volumes", get(sbx_handler::list_volumes))
        .route("/volumes/{volume_id}", delete(sbx_handler::delete_volume))
        .route(
            "/sandboxes/{sandbox_id}/pause",
            post(sbx_handler::not_supported),
        )
        .route(
            "/sandboxes/{sandbox_id}/resume",
            post(sbx_handler::not_supported),
        )
        .route(
            "/sandboxes/{sandbox_id}/snapshots",
            post(sbx_handler::not_supported),
        )
        .route(
            "/sandboxes/{sandbox_id}/metrics",
            get(sbx_handler::not_supported),
        )
        .route(
            "/sandboxes/{sandbox_id}/network",
            put(sbx_handler::not_supported),
        )
        .layer(axum::middleware::from_fn_with_state(
            deps.clone(),
            e2b_auth,
        ))
        .with_state(deps)
}

#[cfg(not(feature = "kubernetes"))]
pub fn sandbox_routes(_deps: SandboxDeps) -> Router {
    Router::new()
}
