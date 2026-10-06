//! Gateway 代理核心：ProxyHttp 实现
//!
//! 完整数据面路由：
//! 1. 路由匹配（matchit）
//! 2. Body 缓冲（数据面 POST 请求）
//! 3. 标识符提取（body / path / session）
//! 4. Cluster cache 查询/ensure
//! 5. 直接路由到 K8s Service FQDN（跳过 Envoy Gateway 的 cluster_header）

use async_trait::async_trait;
use bytes::Bytes;
use matchit::Router;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::cluster_cache::ClusterCache;
use crate::config::GatewayConfig;
use crate::control_plane_client::ControlPlaneClient;
use crate::identifier_extractor::IdentifierExtractor;
use crate::route_table::{DataPlaneRoute, IdentifierSource, RouteType, build_route_table};
use crate::session_resolver::SessionResolver;
use std::collections::HashMap;

/// 请求路由目标
#[derive(Debug, Clone)]
pub enum RouteTarget {
    /// 控制面：透传到 rcoder-control
    ControlPlane,
    /// 数据面：直接路由到 agent_runner K8s Service
    /// 携带 K8s Service FQDN（如 agent-user-123.namespace.svc.cluster.local）
    AgentService(String),
}

/// ProxyHttp 上下文
pub struct GatewayCtx {
    pub start: std::time::Instant,
    pub target: RouteTarget,
    /// 缓冲的 request body（用于 POST 请求的 identifier 提取）
    pub buffered_body: Option<Vec<u8>>,
}

impl Default for GatewayCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl GatewayCtx {
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            target: RouteTarget::ControlPlane,
            buffered_body: None,
        }
    }
}

/// Agent Runner HTTP 端口
const AGENT_HTTP_PORT: u16 = 8086;

/// 请求 body 最大大小（100MB）
const MAX_BODY_SIZE: usize = 100 * 1024 * 1024;

/// Gateway 代理服务
pub struct GatewayProxy {
    pub route_table: Router<RouteType>,
    config: Arc<GatewayConfig>,
    cluster_cache: Arc<ClusterCache>,
    session_resolver: Arc<SessionResolver>,
    cluster_domain: String,
}

impl GatewayProxy {
    pub fn new(config: Arc<GatewayConfig>) -> anyhow::Result<Self> {
        let control_client = ControlPlaneClient::with_configured_key(
            config.control_plane_url.clone(),
            config.control_plane_api_key.clone(),
        );
        let ttl = config.cache_ttl();

        let cluster_cache = Arc::new(ClusterCache::new(control_client.clone(), ttl));
        let session_resolver = Arc::new(SessionResolver::new(control_client, ttl));

        // K8s 集群域名配置
        let cluster_domain = shared_types::get_k8s_cluster_domain();

        info!(
            "[GATEWAY] initialized, control={}, namespace={}, cluster_domain={}",
            config.control_plane_url, config.namespace, cluster_domain
        );

        Ok(Self {
            route_table: build_route_table()
                .map_err(|error| anyhow::anyhow!("build gateway route table: {error}"))?,
            config,
            cluster_cache,
            session_resolver,
            cluster_domain,
        })
    }

    fn resolve_route(&self, path: &str) -> (RouteType, HashMap<String, String>) {
        match self.route_table.at(path) {
            Ok(matched) => {
                let params: HashMap<String, String> = matched
                    .params
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                (matched.value.clone(), params)
            }
            Err(_) => (RouteType::ControlPlane, HashMap::new()),
        }
    }

    /// 提取标识符
    async fn extract_identifier(
        &self,
        route: &DataPlaneRoute,
        path: &str,
        body: Option<&[u8]>,
        path_params: &HashMap<String, String>,
    ) -> Result<Option<String>, shared_types::AppError> {
        let identifier = match &route.source {
            IdentifierSource::Body => {
                body.and_then(|body| IdentifierExtractor::from_body(body, route.identifier_field))
            }
            IdentifierSource::Path(param_name) => {
                IdentifierExtractor::from_path_params(path_params, param_name)
            }
            IdentifierSource::Session => {
                let Some(session_id) = path.split('/').next_back() else {
                    return Ok(None);
                };
                Some(self.session_resolver.resolve(session_id).await?.identifier)
            }
        };
        Ok(identifier)
    }

    /// Service 名称来自控制面核验的物理容器名称，不从业务标识符推算。
    fn build_service_fqdn(&self, container_name: &str) -> String {
        format!(
            "{container_name}-svc.{}.svc.{}",
            self.config.namespace, self.cluster_domain
        )
    }

    async fn write_control_error(
        session: &mut Session,
        error: shared_types::AppError,
    ) -> pingora_core::Result<bool> {
        let status = error.status_code().as_u16();
        let locale = shared_types::parse_accept_language(
            session
                .req_header()
                .headers
                .get("accept-language")
                .and_then(|value| value.to_str().ok()),
        );
        let body = serde_json::to_vec(&error.into_http_result::<()>(locale)).map_err(|error| {
            pingora_core::Error::explain(
                pingora_core::ErrorType::InternalError,
                format!("serialize gateway diagnostic: {error}"),
            )
        })?;
        let mut response = ResponseHeader::build(status, None)?;
        response.insert_header("content-type", "application/json")?;
        response.insert_header("content-length", body.len().to_string())?;
        response.insert_header("cache-control", "no-store")?;
        session
            .write_response_header(Box::new(response), false)
            .await?;
        session
            .write_response_body(Some(Bytes::from(body)), true)
            .await?;
        Ok(true)
    }

    /// 读取 request body（用于 POST 请求），带大小限制
    async fn read_request_body(session: &mut Session) -> Result<Option<Vec<u8>>, &'static str> {
        let mut body = Vec::new();
        loop {
            match session.downstream_session.read_request_body().await {
                Ok(Some(chunk)) => {
                    body.extend_from_slice(&chunk);
                    if body.len() > MAX_BODY_SIZE {
                        return Err("request body too large");
                    }
                }
                Ok(None) => break,
                Err(_) => return Err("failed to read request body"),
            }
        }
        if body.is_empty() {
            Ok(None)
        } else {
            Ok(Some(body))
        }
    }
}

#[async_trait]
impl ProxyHttp for GatewayProxy {
    type CTX = GatewayCtx;

    fn new_ctx(&self) -> Self::CTX {
        GatewayCtx::new()
    }

    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora_core::Result<bool> {
        let path = session.req_header().uri.path().to_string();
        let method = session.req_header().method.as_str();

        let (route_type, path_params) = self.resolve_route(&path);
        match route_type {
            RouteType::GatewayHealth => {
                debug!("[GATEWAY] health check");
                let resp = ResponseHeader::build(200, None)?;
                session.write_response_header(Box::new(resp), false).await?;
                session
                    .write_response_body(Some(Bytes::from("ok")), true)
                    .await?;
                return Ok(true);
            }
            RouteType::ControlPlane => {
                ctx.target = RouteTarget::ControlPlane;
            }
            RouteType::DataPlane(route) => {
                // 缓冲 body（POST 请求需要从 body 提取 identifier）
                let body = if matches!(route.source, IdentifierSource::Body) && method == "POST" {
                    match Self::read_request_body(session).await {
                        Ok(body) => body,
                        Err(e) => {
                            warn!("[GATEWAY] body read error for {}: {}", path, e);
                            ctx.target = RouteTarget::ControlPlane;
                            return Ok(false);
                        }
                    }
                } else {
                    None
                };
                ctx.buffered_body = body.clone();

                // 提取 identifier
                let identifier = self
                    .extract_identifier(&route, &path, body.as_deref(), &path_params)
                    .await;

                let identifier = match identifier {
                    Ok(Some(id)) => id,
                    Err(error) => return Self::write_control_error(session, error).await,
                    Ok(None) => {
                        warn!(
                            "[GATEWAY] failed to extract identifier from {} ({})",
                            path, route.identifier_field
                        );
                        ctx.target = RouteTarget::ControlPlane;
                        return Ok(false);
                    }
                };

                // 只读冷缓存直接转发控制面查询，不进入有副作用的 ensure。
                let container_name = if route.read_only {
                    match self
                        .cluster_cache
                        .get_only(&identifier, route.service_type)
                        .await
                    {
                        Some(name) => name,
                        None => {
                            ctx.target = RouteTarget::ControlPlane;
                            return Ok(false);
                        }
                    }
                } else {
                    match self
                        .cluster_cache
                        .get_or_ensure(&identifier, route.service_type)
                        .await
                    {
                        Ok(name) => name,
                        Err(error) => return Self::write_control_error(session, error).await,
                    }
                };
                let fqdn = self.build_service_fqdn(&container_name);
                debug!("[GATEWAY] {} → {} → agent_svc ({})", path, identifier, fqdn);
                ctx.target = RouteTarget::AgentService(fqdn);
            }
        }
        Ok(false)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora_core::Result<Box<HttpPeer>> {
        match &ctx.target {
            RouteTarget::ControlPlane => {
                let (host, port) = GatewayConfig::parse_addr(&self.config.control_plane_url);
                debug!("[GATEWAY] upstream → {}:{} (control)", host, port);
                Ok(Box::new(HttpPeer::new((host, port), false, String::new())))
            }
            RouteTarget::AgentService(fqdn) => {
                // K8s Service FQDN: port 8086 (agent_runner HTTP)
                debug!("[GATEWAY] upstream → {}:8086 (agent)", fqdn);
                Ok(Box::new(HttpPeer::new(
                    (fqdn.as_str(), AGENT_HTTP_PORT),
                    false,
                    String::new(),
                )))
            }
        }
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> pingora_core::Result<()> {
        match ctx.target {
            RouteTarget::ControlPlane => {
                if let Some(key) = &self.config.control_plane_api_key {
                    upstream_request.insert_header("x-api-key", key)?;
                }
            }
            RouteTarget::AgentService(_) => {
                // Remove only our configured control credential. This Gateway
                // does not authenticate incoming keys; unrelated values keep their meaning.
                if let Some(control_key) = &self.config.control_plane_api_key {
                    let values: Vec<_> = upstream_request
                        .headers
                        .get_all("x-api-key")
                        .iter()
                        .filter(|value| value.as_bytes() != control_key.as_bytes())
                        .cloned()
                        .collect();
                    if values.len() != upstream_request.headers.get_all("x-api-key").iter().count()
                    {
                        drop(upstream_request.remove_header("x-api-key"));
                        for value in values {
                            upstream_request.append_header("x-api-key", value)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// 将缓冲的 request body 回注到 upstream 请求中
    ///
    /// Pingora 的 `read_request_body()` 会消费掉 body（内部调用 `self.body.take()`）。
    /// 如果不在 `request_body_filter` 中回注，upstream 会收到空 body。
    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        _end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> pingora_core::Result<()>
    where
        Self::CTX: Send + Sync,
    {
        if let Some(buffered) = ctx.buffered_body.take() {
            *body = Some(Bytes::from(buffered));
        }
        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> pingora_core::Result<()> {
        let status = upstream_response.status.as_u16();
        let elapsed = ctx.start.elapsed();
        debug!(
            "[GATEWAY] response {} from {:?} in {:.1}ms",
            status,
            ctx.target,
            elapsed.as_secs_f64() * 1000.0
        );
        Ok(())
    }
}

// Append to the saved current pre-fix gateway_proxy.rs. This calls the real
// ProxyHttp callback with a real native TCP Pingora session; no header helper.
#[cfg(test)]
mod configured_control_key_boundary_tests {
    use super::*;
    #[tokio::test]
    async fn gateway_agent_request_never_receives_the_configured_control_key() {
        let proxy = GatewayProxy::new(Arc::new(GatewayConfig {
            gateway_port: 8090,
            control_plane_url: "http://127.0.0.1:8087".into(),
            control_plane_api_key: Some("fixture-primary-control-key".into()),
            envoy_gateway_url: "http://127.0.0.1:8080".into(),
            namespace: "fixture".into(),
            cache_ttl_seconds: 30,
        }))
        .expect("gateway config");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("TCP session");
        let client = tokio::net::TcpStream::connect(listener.local_addr().expect("address"))
            .await
            .expect("client");
        let (stream, _) = listener.accept().await.expect("accepted session");
        let mut session = Session::new_h1(Box::new(
            pingora_core::protocols::l4::stream::Stream::from(stream),
        ));
        let mut ctx = GatewayCtx::new();
        ctx.target = RouteTarget::AgentService("fixture-agent-svc".into());
        let mut request = RequestHeader::build("GET", b"/health", None).expect("request");
        request
            .insert_header("x-api-key", "fixture-primary-control-key")
            .expect("incoming key");
        request
            .insert_header("x-business-correlation", "keep-this-value")
            .expect("business header");
        proxy
            .upstream_request_filter(&mut session, &mut request, &mut ctx)
            .await
            .expect("actual callback");
        assert!(
            !request.headers.contains_key("x-api-key"),
            "the configured RCoder control credential must not reach agent_runner:8086"
        );
        assert_eq!(request.headers["x-business-correlation"], "keep-this-value");
        drop(client);
    }
    #[tokio::test]
    async fn gateway_key_filter_keeps_unknown_keys_and_only_overrides_control_requests() {
        for (agent, configured, incoming, expected) in [
            (
                true,
                Some("configured-control"),
                Some("business-peer-key"),
                Some("business-peer-key"),
            ),
            (
                true,
                None,
                Some("business-peer-key"),
                Some("business-peer-key"),
            ),
            (true, Some("configured-control"), None, None),
            (
                false,
                Some("configured-control"),
                Some("business-peer-key"),
                Some("configured-control"),
            ),
            (
                false,
                None,
                Some("business-peer-key"),
                Some("business-peer-key"),
            ),
        ] {
            let proxy = GatewayProxy::new(Arc::new(GatewayConfig {
                gateway_port: 8090,
                control_plane_url: "http://127.0.0.1:8087".into(),
                control_plane_api_key: configured.map(str::to_owned),
                envoy_gateway_url: "http://127.0.0.1:8080".into(),
                namespace: "fixture".into(),
                cache_ttl_seconds: 30,
            }))
            .expect("gateway config");
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("session listener");
            let client = tokio::net::TcpStream::connect(listener.local_addr().expect("address"))
                .await
                .expect("client");
            let (stream, _) = listener.accept().await.expect("session");
            let mut session = Session::new_h1(Box::new(
                pingora_core::protocols::l4::stream::Stream::from(stream),
            ));
            let mut ctx = GatewayCtx::new();
            if agent {
                ctx.target = RouteTarget::AgentService("fixture-agent-svc".into());
            }
            let mut request = RequestHeader::build("GET", b"/health", None).expect("request");
            if let Some(key) = incoming {
                request
                    .insert_header("x-api-key", key)
                    .expect("incoming key");
            }
            request
                .insert_header("x-business-correlation", "keep-this-value")
                .expect("business header");
            proxy
                .upstream_request_filter(&mut session, &mut request, &mut ctx)
                .await
                .expect("actual callback");
            assert_eq!(
                request
                    .headers
                    .get("x-api-key")
                    .and_then(|value| value.to_str().ok()),
                expected
            );
            assert_eq!(request.headers["x-business-correlation"], "keep-this-value");
            drop(client);
        }
    }
}
