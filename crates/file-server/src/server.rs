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
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(|req: &axum::http::Request<_>| {
                        tracing::info_span!(
                            target: "file_server::http",
                            "http_request",
                            method = %req.method(),
                            uri = %req.uri(),
                        )
                    })
                    .on_request(|req: &axum::http::Request<_>, _span: &tracing::Span| {
                        tracing::info!(
                            target: "file_server::http",
                            method = %req.method(),
                            uri = %req.uri(),
                            "request received"
                        );
                    })
                    .on_response(
                        |res: &Response<_>, latency: std::time::Duration, _span: &tracing::Span| {
                            tracing::info!(
                                target: "file_server::http",
                                status = res.status().as_u16(),
                                latency_ms = latency.as_millis(),
                                "response sent"
                            );
                        },
                    ),
            )
            .with_state(self.state.clone()))
    }

    pub async fn serve(self, listener: tokio::net::TcpListener) -> Result<()> {
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
        let dev_server = self.dev_server_manager();
        let result = axum::serve(listener, self.router()?)
            .with_graceful_shutdown(shutdown)
            .await;
        dev_server.shutdown_all().await;
        result.context("serve file-server")
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
