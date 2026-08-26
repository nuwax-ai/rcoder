// router 整体由 binary (main.rs) 使用，lib 内不直接调用 create_router / ApiDoc 等。
// 抑制 dead_code 以避免 lib 维度误报。
#![allow(dead_code)]

use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::Request,
    middleware::Next,
    response::IntoResponse,
    response::Response,
    routing::{get, post},
};

use crate::handler;
use rcoder_telemetry::{HttpMetricsLayer, TelemetryGuard};

async fn locale_context_middleware(mut req: Request<axum::body::Body>, next: Next) -> Response {
    let locale = shared_types::parse_accept_language(
        req.headers()
            .get("accept-language")
            .and_then(|v| v.to_str().ok()),
    );

    req.extensions_mut().insert(locale);

    shared_types::scope_request_locale(locale, async move { next.run(req).await }).await
}

/// 内部 API 路由（供 rcoder-gateway 调用）
///
/// 这些端点挂载在中间件之前，绕过 API Key 鉴权。
fn create_internal_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/internal/pod/ensure", post(handler::internal_pod_ensure))
        .route(
            "/internal/session/{session_id}/resolve",
            get(handler::internal_session_resolve),
        )
        .with_state(state)
}

/// 创建 Axum 路由
// AppState/SessionInfo 拆至 app_state.rs（状态+会话注册表自成一档）；
// re-export 保持 crate::router::AppState 既有引用稳定。
pub use crate::app_state::AppState;

pub fn create_router(state: Arc<AppState>, telemetry: Option<Arc<TelemetryGuard>>) -> Router {
    let api_routes = Router::new()
        .route("/chat", post(handler::handle_chat))
        // Axum SSE 代理处理器，直接返回 SSE 流
        .route(
            "/agent/progress/{session_id}",
            get(handler::agent_session_notification),
        )
        .route("/agent/session/cancel", post(handler::agent_session_cancel))
        .route(
            "/agent/notify-resolved",
            post(handler::agent_notify_resolved),
        )
        .route("/agent/stop", post(handler::agent_stop))
        .route("/agent/status/{project_id}", get(handler::agent_status))
        .with_state(state.clone());

    // Computer Agent Runner 路由
    let computer_routes = Router::new()
        .route("/computer/chat", post(handler::handle_computer_chat))
        .route("/computer/agent/stop", post(handler::computer_agent_stop))
        .route(
            "/computer/agent/status",
            post(handler::computer_agent_status),
        ) // 🆕 新增
        .route(
            "/computer/agent/session/cancel",
            post(handler::computer_agent_session_cancel),
        )
        .route(
            "/computer/notify-resolved",
            post(handler::computer_notify_resolved),
        )
        // 进度流复用现有的 agent_session_notification
        .route(
            "/computer/progress/{session_id}",
            get(handler::computer_agent_progress_notification),
        )
        // VNC 桌面访问说明接口
        .route(
            "/computer/desktop/{user_id}/{project_id}",
            get(handler::computer_desktop_vnc),
        )
        .route(
            "/computer/desktop-proxy/{user_id}/{project_id}/{*path}",
            get(handler::computer_desktop_proxy),
        )
        .route(
            "/computer/ttyd/{user_id}/{project_id}/{*path}",
            get(handler::computer_ttyd_proxy),
        )
        // Pod 容器管理接口
        .route("/computer/pod/count", get(handler::pod_count))
        .route("/computer/pod/list", get(handler::pod_list))
        .route("/computer/pod/ensure", post(handler::pod_ensure))
        .route("/computer/pod/keepalive", post(handler::pod_keepalive))
        .route("/computer/pod/restart", post(handler::pod_restart))
        .route("/computer/pod/status", get(handler::pod_status))
        .route("/computer/pod/vnc-status", get(handler::pod_vnc_status))
        // 🆕 音频代理路由（用于 OpenAPI 文档）
        .route(
            "/computer/audio/{user_id}/{project_id}/{*path}",
            get(handler::computer_audio_proxy),
        )
        // 🆕 IME 代理路由（用于 OpenAPI 文档）
        .route(
            "/computer/ime/{user_id}/{project_id}/{*path}",
            get(handler::computer_ime_proxy),
        )
        // 🆕 Computer Agent-runner 容器 PG 管理（重置密码 / 建库; rcoder exec 容器内 psql）
        .route(
            "/computer/db/{user_id}/reset-password",
            post(handler::computer_db_reset_password),
        )
        .route(
            "/computer/db/{user_id}/create-database",
            post(handler::computer_db_create_database),
        )
        .route("/computer/cache/clean", post(handler::computer_cache_clean))
        .with_state(state.clone());

    // Pingora 代理 API 路由（用于文档和状态查询）
    let proxy_api_routes = Router::new()
        .route("/proxy/status", get(handler::proxy_status))
        .route("/proxy/stats", get(handler::proxy_stats))
        .route("/proxy/config", get(handler::proxy_config))
        // userApp 工具族 307 文档接口（实际流量走 Pingora 8088；stage 段 dev/prod 统一：
        // 此处提供 Swagger 文档 + 可直接调用的重定向语义，对齐 devapp 先例）
        // 开发域（UserAppBuilder 开发容器）：ttyd/vnc/audio/ime/dbx
        .route(
            "/userapp/dev/ttyd/{app_id}/{*path}",
            get(handler::proxy_to_userapp_ttyd),
        )
        .route(
            "/userapp/dev/ttyd/{app_id}",
            get(handler::proxy_to_userapp_ttyd_redirect_root),
        )
        .route(
            "/userapp/dev/vnc/{app_id}/{*path}",
            get(handler::proxy_to_userapp_vnc),
        )
        .route(
            "/userapp/dev/vnc/{app_id}",
            get(handler::proxy_to_userapp_vnc_redirect_root),
        )
        .route(
            "/userapp/dev/audio/{app_id}/{*path}",
            get(handler::proxy_to_userapp_audio),
        )
        .route(
            "/userapp/dev/audio/{app_id}",
            get(handler::proxy_to_userapp_audio_redirect_root),
        )
        .route(
            "/userapp/dev/ime/{app_id}/{*path}",
            get(handler::proxy_to_userapp_ime),
        )
        .route(
            "/userapp/dev/ime/{app_id}",
            get(handler::proxy_to_userapp_ime_redirect_root),
        )
        .route(
            "/userapp/dev/dbx/{app_id}/{*path}",
            get(handler::proxy_to_dev_dbx),
        )
        .route(
            "/userapp/dev/dbx/{app_id}",
            get(handler::proxy_to_dev_dbx_redirect_root),
        )
        // 生产域（运行容器，部署后的生产环境）：ttyd/pgweb/dbx
        .route(
            "/userapp/prod/ttyd/{app_id}/{*path}",
            get(handler::proxy_to_userapp_runtime_ttyd),
        )
        .route(
            "/userapp/prod/ttyd/{app_id}",
            get(handler::proxy_to_userapp_runtime_ttyd_redirect_root),
        )
        .route(
            "/userapp/prod/pgweb/{app_id}/{*path}",
            get(handler::proxy_to_userapp_runtime_pgweb),
        )
        .route(
            "/userapp/prod/pgweb/{app_id}",
            get(handler::proxy_to_userapp_runtime_pgweb_redirect_root),
        )
        .route(
            "/userapp/prod/dbx/{app_id}/{*path}",
            get(handler::proxy_to_prod_dbx),
        )
        .route(
            "/userapp/prod/dbx/{app_id}",
            get(handler::proxy_to_prod_dbx_redirect_root),
        )
        .route("/userapp/routes", get(handler::userapp_proxy_routes_doc))
        .with_state(state.clone());

    // DevComputer 调试路由 — 委托给 /computer/* 处理器，共享同一个容器
    let devcomputer_routes = Router::new()
        .route("/devcomputer/chat", post(handler::handle_devcomputer_chat))
        .route(
            "/devcomputer/agent/stop",
            post(handler::devcomputer_agent_stop),
        )
        .route(
            "/devcomputer/agent/status",
            post(handler::devcomputer_agent_status),
        )
        .route(
            "/devcomputer/agent/session/cancel",
            post(handler::devcomputer_agent_session_cancel),
        )
        .route(
            "/devcomputer/notify-resolved",
            post(handler::devcomputer_notify_resolved),
        )
        .route(
            "/devcomputer/progress/{session_id}",
            get(handler::devcomputer_agent_progress_notification),
        )
        .with_state(state.clone());

    // 调试路由（仅用于开发和问题排查，需要 feature flag "debug" 启用）
    #[cfg(feature = "debug")]
    let debug_routes = Router::new()
        .route("/debug/sql", get(handler::debug_dump_summary))
        .route("/debug/projects", get(handler::debug_list_projects))
        .route("/debug/containers", get(handler::debug_list_containers))
        .route("/debug/storage/stats", get(handler::debug_storage_stats))
        .with_state(state.clone());

    // 健康检查路由
    let health_routes = Router::new()
        .route("/health", get(handler::health_check))
        .with_state(state.clone());

    // P0-5: Agent Management 路由(全部 POST + body 解析)
    // - 简单 JSON 端点使用 I18nJsonOrQuery(同时支持 JSON body 和 ?project_id=xxx query)
    // - install 端点使用 multipart/form-data(file + metadata JSON 字段)
    //
    // ⚠️ install 路由的 body 限制必须在 Router 层挂,而不是 MethodRouter 层。
    // axum 的 `Multipart` 提取器通过 `with_limited_body()` 读取
    // `DefaultBodyLimitKind` 扩展(Request 上挂的 layer 才生效),`MethodRouter::layer`
    // 出来的 MethodRouter 不携带这个扩展,无法被 multipart 识别。
    // 此外 `RequestBodyLimitLayer` 是 tower 中间件,只读取 Content-Length 头,
    // 对 streaming 的 multipart body 不直接生效,但保留作为 defense-in-depth。
    let install_route = Router::new()
        .route("/agent-mgmt/agents/install", post(handler::install_agent))
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            1024 * 1024 * 1024,
        ));

    let agent_mgmt_routes = Router::new()
        .route("/agent-mgmt/agents/list", post(handler::list_agents))
        .route("/agent-mgmt/agents/get", post(handler::get_agent))
        .route("/agent-mgmt/agents/check", post(handler::check_agent))
        .merge(install_route)
        .route(
            "/agent-mgmt/agents/install-from-url",
            post(handler::install_from_url),
        )
        .route(
            "/agent-mgmt/agents/install-from-npm",
            post(handler::install_from_npm),
        )
        .route(
            "/agent-mgmt/agents/uninstall",
            post(handler::uninstall_agent),
        )
        .with_state(state.clone());

    // 应用管理路由
    let app_manager_state = Arc::new(app_manager::handlers::AppManagerState {
        app_service: state.app_service.clone(),
        // 共享客户端 (连接超时 + 连接池复用; SSE 流不能设总超时, 见 http_client 模块)
        http_client: crate::http_client::shared_client().clone(),
    });
    let app_manager_routes = app_manager::routes::app_manager_routes()
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024)) // 1GiB（upload 压缩包，覆盖全局 50MB）
        .with_state(app_manager_state);

    // userApp 文件域转发层: /api/userapp/{*rest} 通配透传 + create-workspace 显式入口
    // （build/tasks/static 等构建链接口均在 file-server 侧，经此转发直达 builder）
    let userapp_forward_routes = crate::userapp_forward::routes().with_state(state.clone());

    // file-server 分流代理运行时启停 (无 state, 受全局 API key 中间件保护;
    // `rcoder file-server {start,stop,restart,status}` CLI 的服务端)
    let file_server_admin_routes = crate::file_server_admin::admin_routes();

    let mut router = Router::new()
        .merge(health_routes)
        .merge(api_routes)
        .merge(computer_routes)
        .merge(devcomputer_routes)
        .merge(proxy_api_routes)
        .merge(agent_mgmt_routes)
        .merge(app_manager_routes)
        .merge(userapp_forward_routes)
        .merge(file_server_admin_routes);

    // 仅在启用 debug feature 时添加调试路由
    #[cfg(feature = "debug")]
    {
        router = router.merge(debug_routes);
    }

    // 添加 /metrics 端点（如果启用了 Prometheus）
    if let Some(ref guard) = telemetry {
        let guard_clone = Arc::clone(guard);
        router = router.route(
            "/metrics",
            get(move || {
                let guard = Arc::clone(&guard_clone);
                async move { metrics_handler(guard).await }
            }),
        );
    }

    // 🆕 克隆共享的 API Key 配置用于中间件
    let api_key_config = Arc::clone(&state.api_key_config);

    router
        .merge(crate::router_docs::create_swagger_ui())
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024)) // 50MB body 大小限制
        // HTTP 请求日志（target: tower_http → rcoder.log）+ W3C traceparent 提取
        // （入站 trace 贯通：e2e 注入 traceparent 时请求 span 继承远端 trace）
        .layer(
            tower_http::trace::TraceLayer::new_for_http().make_span_with(
                |req: &Request<axum::body::Body>| {
                    rcoder_telemetry::make_span_with_trace_parent(req)
                },
            ),
        )
        .layer(HttpMetricsLayer::new()) // HTTP 指标中间件
        // API Key 鉴权中间件（支持热更新）
        .layer(axum::middleware::from_fn(move |req, next| {
            crate::middleware::api_key_middleware::api_key_middleware_handler(
                Arc::clone(&api_key_config),
                req,
                next,
            )
        }))
        .layer(axum::middleware::from_fn(locale_context_middleware))
        // 内部 API（供 rcoder-gateway 调用，绕过 API Key 鉴权）
        .merge(create_internal_routes(state.clone()))
        // file-server 基础路由（TS 移植版老路径：/api/project、/api/computer、/api/git、
        // /api/build、/api/page；排除 /api/userapp——由 rcoder 转发层接管）。
        // 与 TS 行为一致不设 API key → merge 在 api-key layer 之后（同 internal 先例）；
        // 构造失败不阻断主服务启动（warn 可见，缺路由面可诊断）。
        // computer 域拦截层：header X-Service-Type=userapp 的请求短路转发到该 app
        // 开发容器（反向代理转来的 TS 老路径，body 零解析）。
        .merge(match crate::file_server_embed::merged_router() {
            Ok(fs_router) => fs_router.layer(axum::middleware::from_fn_with_state(
                state.clone(),
                crate::userapp_forward::computer_intercept,
            )),
            Err(e) => {
                tracing::warn!("file-server routes not mounted on main service: {e}");
                Router::new()
            }
        })
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("x-frame-options"),
            axum::http::HeaderValue::from_static("DENY"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("x-content-type-options"),
            axum::http::HeaderValue::from_static("nosniff"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("strict-transport-security"),
            axum::http::HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("referrer-policy"),
            axum::http::HeaderValue::from_static("strict-origin-when-cross-origin"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("x-xss-protection"),
            axum::http::HeaderValue::from_static("0"),
        ))
}

/// Prometheus 指标处理器
async fn metrics_handler(telemetry: Arc<TelemetryGuard>) -> impl IntoResponse {
    match telemetry.render_metrics() {
        Some(metrics) => (
            axum::http::StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            metrics,
        ),
        None => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            "Prometheus metrics not enabled".to_string(),
        ),
    }
}
