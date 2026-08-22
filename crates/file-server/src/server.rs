//! 可嵌入的 file-server 组装与服务入口。

use std::future::Future;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::{DefaultBodyLimit, Request};
use axum::http::HeaderValue;
use axum::middleware::{Next, from_fn};
use axum::response::{IntoResponse, Response};
use tower_http::trace::TraceLayer;
use tracing::info;

use crate::error::{AppError, REQUEST_ID, generate_request_id};
use crate::{
    AppState, BuildManager, BuildTaskStore, Config, DevServerManager, LocalWorkspaceResolver,
    LogCacheManager, SkillDownloader, WorkspaceResolver,
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
            build_tasks: Arc::new(BuildTaskStore::new()),
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

    pub fn router(&self) -> Result<Router> {
        let request_body_limit = usize::try_from(self.state.config.request_body_max_bytes)
            .context("REQUEST_BODY_MAX_BYTES exceeds platform usize")?;
        let (api_router, openapi) = crate::routes::api_router().split_for_parts();
        Ok(api_router
            .merge(crate::openapi::swagger_ui(openapi))
            .fallback(not_found)
            .layer(DefaultBodyLimit::max(request_body_limit))
            .layer(from_fn(request_id_layer))
            .layer(from_fn(locale_layer))
            // userApp 分流标记（X-Service-Type=userapp → task-local；computer 域
            // workspace 定位据此切开发卷）——容器内主场景层
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
            .with_state(self.state.clone()))
    }

    /// 合并进 rcoder 主服务用的基础路由（[`crate::routes::api_router_base`]）。
    ///
    /// 与 [`Self::router`] 的差异：不含 swagger UI、不设 fallback（axum 不允许
    /// 双 fallback merge 进主 Router）、不含 `/`、`/health`、`/api/userapp`
    /// （排除原因见 `routes::api_router_base` 文档）。中间件层（body limit/
    /// request_id/locale/请求日志/TraceLayer）随子路由生效于本子树。
    pub fn router_base(&self) -> Result<Router> {
        let request_body_limit = usize::try_from(self.state.config.request_body_max_bytes)
            .context("REQUEST_BODY_MAX_BYTES exceeds platform usize")?;
        let (api_router, _openapi) = crate::routes::api_router_base().split_for_parts();
        Ok(api_router
            .layer(DefaultBodyLimit::max(request_body_limit))
            .layer(from_fn(request_id_layer))
            .layer(from_fn(locale_layer))
            // userApp 分流标记同 router()（防御性）：内嵌模式下带 X-Service-Type
            // 的请求按约定被 rcoder 拦截层短路，不会进入本地 handler——但这是
            // 跨 crate 的隐式顺序约定，无编译器保证；带上此层后即使约定被打破，
            // 分流也只是 no-op（is_userapp_request 恒 false）而非行为漂移。
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
            .with_state(self.state.clone()))
    }

    pub async fn serve(self, listener: tokio::net::TcpListener) -> Result<()> {
        self.log_startup(&listener);
        let dev_server = self.dev_server_manager();
        let result = axum::serve(listener, self.router()?).await;
        dev_server.shutdown_all().await;
        result.context("serve file-server")
    }

    pub async fn serve_with_shutdown<F>(
        self,
        listener: tokio::net::TcpListener,
        shutdown: F,
    ) -> Result<()>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.log_startup(&listener);
        let dev_server = self.dev_server_manager();
        let result = axum::serve(listener, self.router()?)
            .with_graceful_shutdown(shutdown)
            .await;
        dev_server.shutdown_all().await;
        result.context("serve file-server")
    }

    /// 启动留痕（target=file_server::server → 嵌入模式进独立日志文件）:
    /// 版本 + 部署模式 + 三个 workspace 根（排查路径问题的第一手信息）。
    fn log_startup(&self, listener: &tokio::net::TcpListener) {
        let addr = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        info!(
            version = crate::VERSION,
            deployment_mode = ?self.state.config.deployment_mode,
            project_source_dir = %self.state.config.project_source_dir.display(),
            computer_workspace_dir = %self.state.config.computer_workspace_dir.display(),
            userapp_workspace_dir = %self.state.config.userapp_workspace_dir.display(),
            addr = %addr,
            "file-server started"
        );
    }
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
