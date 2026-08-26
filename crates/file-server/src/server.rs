//! 可嵌入的 file-server 组装与服务入口。

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::{DefaultBodyLimit, Request};
use axum::http::HeaderValue;
use axum::middleware::{Next, from_fn};
use axum::response::{IntoResponse, Response};
use tower_http::trace::TraceLayer;

use crate::error::{AppError, REQUEST_ID, generate_request_id};
use crate::{
    AppState, BuildManager, Config, DevServerManager, LocalWorkspaceResolver, LogCacheManager,
    SkillDownloader, WorkspaceResolver,
};

pub struct FileServerBuilder {
    config: Config,
    resolver: Option<Arc<dyn WorkspaceResolver>>,
}

impl FileServerBuilder {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            resolver: None,
        }
    }

    pub fn with_workspace_resolver(mut self, resolver: Arc<dyn WorkspaceResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    pub fn build(self) -> Result<FileServer> {
        self.config
            .validate()
            .context("validate file-server config")?;
        let config = Arc::new(self.config);
        let resolver = self.resolver.unwrap_or_else(|| {
            Arc::new(LocalWorkspaceResolver::new(
                config.project_source_dir.clone(),
                config.computer_workspace_dir.clone(),
            ))
        });
        let skill_downloader = Arc::new(
            SkillDownloader::new(&config)
                .map_err(|error| anyhow::anyhow!("initialize secure skill downloader: {error}"))?,
        );
        let state = AppState {
            resolver,
            dev_server: Arc::new(DevServerManager::new(config.clone())),
            build_manager: Arc::new(BuildManager::new(config.max_build_concurrency)),
            log_cache: Arc::new(LogCacheManager::new(&config)),
            skill_downloader,
            config,
            started_at: std::time::Instant::now(),
        };
        Ok(FileServer { state })
    }
}

#[derive(Clone)]
pub struct FileServer {
    state: AppState,
}

impl FileServer {
    pub fn builder(config: Config) -> FileServerBuilder {
        FileServerBuilder::new(config)
    }

    pub fn config(&self) -> &Config {
        &self.state.config
    }

    pub fn dev_server_manager(&self) -> Arc<DevServerManager> {
        self.state.dev_server.clone()
    }

    /// 共享状态句柄（file-server-userapp 组装 full/container Router 用：
    /// 其 UserAppState 持本 state 的 Arc 引用共享单例设施）。
    pub fn state(&self) -> AppState {
        self.state.clone()
    }

    pub fn router(&self) -> Result<Router> {
        let request_body_limit = usize::try_from(self.state.config.request_body_max_bytes)
            .context("REQUEST_BODY_MAX_BYTES exceeds platform usize")?;
        let (api_router, openapi) = crate::routes::api_router().split_for_parts();
        let routed = api_router
            .merge(crate::openapi::swagger_ui(openapi))
            .fallback(not_found);
        // userApp 分流标记（X-Service-Type=userapp → task-local；computer 域
        // workspace 定位据此切开发卷）——容器内主场景层，含于公共栈
        Ok(apply_common_layers(routed, request_body_limit).with_state(self.state.clone()))
    }

    /// 合并进 rcoder 主服务用的基础路由（[`crate::routes::api_router_base`]）。
    ///
    /// 与 [`Self::router`] 的差异：不含 swagger UI、不设 fallback（axum 不允许
    /// 双 fallback merge 进主 Router）、不含 `/`、`/health`、`/api/userapp`
    /// （排除原因见 `routes::api_router_base` 文档）。中间件层（body limit/
    /// request_id/locale/请求日志/TraceLayer）随子路由生效于本子树。
    /// agent-runner 开发容器内嵌形态（[`crate::routes::api_router_container`]）：
    /// 全量业务路由**含 `/api/userapp`**（容器是 userApp 域本地实现的宿主），
    /// 不含 swagger/fallback/`/`/`/health`（与宿主 agent_runner 冲突）。
    /// 中间件栈与 [`Self::router_base`] 一致（含 scope_userapp_flag——容器内
    /// X-Service-Type 切开发卷的主场景层）。
    pub fn router_container(&self) -> Result<Router> {
        let request_body_limit = usize::try_from(self.state.config.request_body_max_bytes)
            .context("REQUEST_BODY_MAX_BYTES exceeds platform usize")?;
        let (api_router, _openapi) = crate::routes::api_router_container().split_for_parts();
        Ok(apply_common_layers(api_router, request_body_limit).with_state(self.state.clone()))
    }

    pub fn router_base(&self) -> Result<Router> {
        let request_body_limit = usize::try_from(self.state.config.request_body_max_bytes)
            .context("REQUEST_BODY_MAX_BYTES exceeds platform usize")?;
        let (api_router, _openapi) = crate::routes::api_router_base().split_for_parts();
        // userApp 分流标记含于公共栈（防御性）：内嵌模式下带 X-Service-Type
        // 的请求按约定被 rcoder 拦截层短路，不会进入本地 handler——但这是
        // 跨 crate 的隐式顺序约定，无编译器保证；带上此层后即使约定被打破，
        // 分流也只是 no-op（is_userapp_request 恒 false）而非行为漂移。
        Ok(apply_common_layers(api_router, request_body_limit).with_state(self.state.clone()))
    }

    // serve/serve_with_shutdown 已随独立 bin 删除（npm 分发统一走
    // file-server-proxy 的全量组装形态，见 file_server_userapp::full_router）。
}

/// 公共中间件栈（body limit → request_id → locale → userApp 分流标记 → 请求日志
/// → TraceLayer；后添加的层在最外层）。
///
/// `router()`/`router_container()`/`router_base()` 三形态与 file-server-userapp
/// 组装的 userapp 子树共用本函数——**单一事实源**，中间件演进只改这里。
/// 泛型于 state 类型（from_fn 中间件不依赖 state），调用方各自 `with_state`。
/// `scope_userapp_flag` 对 userapp 子树是 no-op（其 handler 不读该 flag），包含
/// 无行为差异。
pub fn apply_common_layers<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    request_body_limit: usize,
) -> Router<S> {
    router
        .layer(DefaultBodyLimit::max(request_body_limit))
        .layer(from_fn(request_id_layer))
        .layer(from_fn(locale_layer))
        .layer(from_fn(crate::extract::scope_userapp_flag))
        .layer(from_fn(request_log_layer))
        .layer(
            TraceLayer::new_for_http().make_span_with(|req: &axum::http::Request<_>| {
                tracing::info_span!(
                    target: "file_server::http",
                    "http_request",
                    method = %req.method(),
                    uri = %req.uri(),
                )
            }),
        )
}

async fn request_id_layer(req: Request, next: Next) -> Response {
    let id = generate_request_id();
    let response = REQUEST_ID.scope(id.clone(), next.run(req)).await;
    let mut response = response;
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

/// 解析 `Accept-Language` 头 → 注入请求级 locale task-local, 供 AppError i18n 翻译。
/// 放在最外层 (在 request_id_layer 之外), 确保所有错误响应都能拿到 locale。
async fn locale_layer(req: Request, next: Next) -> Response {
    let accept_lang = req
        .headers()
        .get("accept-language")
        .and_then(|v| v.to_str().ok());
    let locale = shared_types::parse_accept_language(accept_lang);
    shared_types::scope_request_locale(locale, next.run(req)).await
}

/// 请求日志中间件: 记录每个请求的 method/uri/status/latency。
/// 用 `file_server` target 确保写入文件日志 (file_layer 只收集 file_server target)。
/// X-Service-Type / X-App-Id: userApp 分流 header（反代/Java 注入），排障关键信息。
async fn request_log_layer(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let uri = req.uri().to_string();
    let service_type = header_str(&req, "x-service-type");
    let app_id = header_str(&req, "x-app-id");
    let start = std::time::Instant::now();
    let response = next.run(req).await;
    tracing::info!(
        method = %method,
        uri = %uri,
        service_type = service_type.as_deref(),
        app_id = app_id.as_deref(),
        status = response.status().as_u16(),
        latency_ms = start.elapsed().as_millis(),
        "request completed"
    );
    response
}

fn header_str(req: &Request, name: &str) -> Option<String> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

async fn not_found(req: Request) -> Response {
    AppError::resource(format!("Path not found: {}", req.uri().path())).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_builder_exposes_router_without_process_globals() {
        let server = FileServer::builder(Config::default())
            .build()
            .expect("build embedded file-server");
        let _router = server.router().expect("build embedded router");
    }
}
