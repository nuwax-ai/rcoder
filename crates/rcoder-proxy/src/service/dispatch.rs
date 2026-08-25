//! 路由分派（从 proxy_http.rs 拆出——request 重写/上游选择两大 match 自成一档）。
//!
//! Pingora 生命周期两个回调（`upstream_request_filter` / `upstream_peer`）各自的
//! RouteType → handler 分派表；新增路由两处都要接线（router.rs 注册 + 本文件两 match）。

use matchit::Params;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;

use crate::service::PortProxy;
use tracing::debug;

use crate::RouteType;
use crate::service::handlers;
use crate::service::types::TrackingCtx;

type ProxyResult<T> = pingora_core::Result<T>;

impl PortProxy {
    /// upstream_request_filter 阶段：按 RouteType 重写发往上游的请求。
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn dispatch_upstream_request(
        &self,
        route: RouteType,
        params: Params<'_, '_>,
        upstream_request: &mut RequestHeader,
        original_uri: &http::Uri,
        ctx: &mut TrackingCtx,
        path: &str,
    ) -> ProxyResult<()> {
        match route {
            RouteType::VncProxy => {
                handlers::vnc::handle_vnc_request(upstream_request, original_uri, params, ctx)
                    .await?;
            }
            RouteType::PortProxy => {
                handlers::port_proxy::handle_port_proxy_request(
                    upstream_request,
                    original_uri,
                    params,
                    self.use_round_robin,
                )
                .await?;
            }
            RouteType::AppPortProxy => {
                handlers::app_port_proxy::handle_app_port_proxy_request(
                    upstream_request,
                    original_uri,
                    params,
                )
                .await?;
            }
            RouteType::DevPortProxy => {
                handlers::dev_port_proxy::handle_dev_port_proxy_request(
                    upstream_request,
                    original_uri,
                    params,
                )
                .await?;
            }
            RouteType::HealthCheck => {
                // 健康检查：代理到 Axum 的 /health 端点
                // 这样既能验证 Pingora 正常运行，又能验证 Axum 正常运行
                debug!(
                    "Health check request: {} - proxying to Axum ({})",
                    path, self.default_backend_port
                );

                // 修改请求路径为 /health
                let health_uri = http::Uri::from_static("/health");
                upstream_request.set_uri(health_uri);

                // 设置目标端口为默认后端端口 (Axum)
                ctx.target_port = Some(self.default_backend_port);
            }
            RouteType::ApiProxy => {
                handlers::api_proxy::handle_api_proxy_request(
                    upstream_request,
                    original_uri,
                    params,
                    ctx,
                    &self.api_key_manager,
                )
                .await?;
            }
            RouteType::AudioProxy => {
                handlers::audio::handle_audio_request(
                    upstream_request,
                    original_uri,
                    params,
                    ctx,
                    &self.vnc_backends,
                )
                .await?;
            }
            RouteType::ImeProxy => {
                handlers::ime::handle_ime_request(
                    upstream_request,
                    original_uri,
                    params,
                    ctx,
                    &self.vnc_backends,
                )
                .await?;
            }
            RouteType::TtydProxy => {
                handlers::ttyd::handle_ttyd_request(upstream_request, original_uri, params, ctx)
                    .await?;
            }
            RouteType::WebTtydProxy => {
                handlers::ttyd::handle_web_ttyd_request(
                    upstream_request,
                    original_uri,
                    params,
                    &self.container_lookup,
                )
                .await?;
            }
            RouteType::DevTtydProxy => {
                handlers::dev_terminal::handle_dev_ttyd_request(
                    upstream_request,
                    original_uri,
                    params,
                    ctx,
                )
                .await?;
            }
            RouteType::DevVncProxy => {
                handlers::dev_terminal::handle_dev_vnc_request(
                    upstream_request,
                    original_uri,
                    params,
                    ctx,
                )
                .await?;
            }
            RouteType::DevAudioProxy => {
                handlers::dev_terminal::handle_dev_audio_request(
                    upstream_request,
                    original_uri,
                    params,
                    ctx,
                    &self.metrics,
                    &self.container_lookup,
                )
                .await?;
            }
            RouteType::DevImeProxy => {
                handlers::dev_terminal::handle_dev_ime_request(
                    upstream_request,
                    original_uri,
                    params,
                    ctx,
                    &self.metrics,
                    &self.container_lookup,
                )
                .await?;
            }
            RouteType::RuntimeTtydProxy => {
                handlers::dev_terminal::handle_runtime_ttyd_request(
                    upstream_request,
                    original_uri,
                    params,
                    ctx,
                )
                .await?;
            }
            RouteType::RuntimePgwebProxy => {
                handlers::dev_terminal::handle_runtime_pgweb_request(
                    upstream_request,
                    original_uri,
                    params,
                    ctx,
                )
                .await?;
            }
            RouteType::DevDbxProxy => {
                handlers::dbx::handle_dev_dbx_request(upstream_request, original_uri, params, ctx)
                    .await?;
            }
            RouteType::ProdDbxProxy => {
                handlers::dbx::handle_prod_dbx_request(upstream_request, original_uri, params, ctx)
                    .await?;
            }
        }

        Ok(())
    }

    /// upstream_peer 阶段：按 RouteType 选择上游 peer。
    pub(crate) async fn dispatch_upstream_peer(
        &self,
        route: RouteType,
        params: Params<'_, '_>,
        ctx: &mut TrackingCtx,
    ) -> ProxyResult<Box<HttpPeer>> {
        match route {
            RouteType::VncProxy => {
                handlers::vnc::handle_vnc_upstream(
                    ctx,
                    params,
                    &self.vnc_backends,
                    &self.metrics,
                    &self.container_lookup,
                )
                .await
            }
            RouteType::PortProxy => {
                handlers::port_proxy::handle_port_proxy_upstream(
                    ctx,
                    params,
                    &self.backends,
                    &self.backend_host,
                    &self.metrics,
                )
                .await
            }
            RouteType::AppPortProxy => {
                handlers::app_port_proxy::handle_app_port_proxy_upstream(
                    ctx,
                    params,
                    &self.app_backends,
                    &self.metrics,
                )
                .await
            }
            RouteType::DevPortProxy => {
                handlers::dev_port_proxy::handle_dev_port_proxy_upstream(
                    ctx,
                    params,
                    &self.metrics,
                    &self.container_lookup,
                )
                .await
            }
            RouteType::HealthCheck => {
                // 健康检查已在 upstream_request_filter 中设置 target_port
                // 这里返回对应的后端 peer
                let target_port = ctx.target_port.unwrap_or(self.default_backend_port);

                // 记录指标
                self.metrics.record_request();
                self.metrics.inc_active();

                // 返回 Axum 服务的 peer
                let peer = Box::new(HttpPeer::new(
                    ("127.0.0.1", target_port),
                    false,
                    "".to_string(),
                ));

                Ok(peer)
            }
            RouteType::ApiProxy => {
                handlers::api_proxy::handle_api_proxy_upstream(
                    ctx,
                    params,
                    &self.api_key_manager,
                    &self.metrics,
                )
                .await
            }
            RouteType::AudioProxy => {
                handlers::audio::handle_audio_upstream(
                    ctx,
                    params,
                    &self.vnc_backends,
                    &self.metrics,
                )
                .await
            }
            RouteType::ImeProxy => {
                handlers::ime::handle_ime_upstream(ctx, params, &self.vnc_backends, &self.metrics)
                    .await
            }
            RouteType::TtydProxy => {
                handlers::ttyd::handle_ttyd_upstream(
                    ctx,
                    params,
                    &self.vnc_backends,
                    &self.metrics,
                    &self.container_lookup,
                )
                .await
            }
            RouteType::WebTtydProxy => {
                handlers::ttyd::handle_web_ttyd_upstream(
                    ctx,
                    params,
                    &self.vnc_backends,
                    &self.project_backends,
                    &self.metrics,
                    &self.container_lookup,
                )
                .await
            }
            RouteType::DevTtydProxy => {
                handlers::dev_terminal::handle_dev_ttyd_upstream(
                    ctx,
                    params,
                    &self.metrics,
                    &self.container_lookup,
                )
                .await
            }
            RouteType::DevVncProxy => {
                handlers::dev_terminal::handle_dev_vnc_upstream(
                    ctx,
                    params,
                    &self.metrics,
                    &self.container_lookup,
                )
                .await
            }
            RouteType::DevAudioProxy => {
                handlers::dev_terminal::handle_dev_audio_upstream(ctx, &self.metrics).await
            }
            RouteType::DevImeProxy => {
                handlers::dev_terminal::handle_dev_ime_upstream(ctx, &self.metrics).await
            }
            RouteType::RuntimeTtydProxy => {
                handlers::dev_terminal::handle_runtime_ttyd_upstream(
                    ctx,
                    params,
                    &self.metrics,
                    &self.container_lookup,
                    &self.app_runtime_ip_slot,
                )
                .await
            }
            RouteType::RuntimePgwebProxy => {
                handlers::dev_terminal::handle_runtime_pgweb_upstream(
                    ctx,
                    params,
                    &self.metrics,
                    &self.container_lookup,
                    &self.app_runtime_ip_slot,
                )
                .await
            }
            RouteType::DevDbxProxy => {
                handlers::dbx::handle_dev_dbx_upstream(
                    ctx,
                    params,
                    &self.metrics,
                    &self.container_lookup,
                )
                .await
            }
            RouteType::ProdDbxProxy => {
                handlers::dbx::handle_prod_dbx_upstream(
                    ctx,
                    params,
                    &self.metrics,
                    &self.container_lookup,
                    &self.app_runtime_ip_slot,
                )
                .await
            }
        }
    }
}
