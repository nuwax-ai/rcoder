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
use pingora_http::ResponseHeader;
use pingora_proxy::{ProxyHttp, Session};
use std::sync::Arc;
use tracing::{debug, error, info, warn};

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

use shared_types::ServiceType;

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
}

impl GatewayProxy {
    pub fn new(config: Arc<GatewayConfig>) -> Self {
        let control_client = ControlPlaneClient::new(config.control_plane_url.clone());
        let ttl = config.cache_ttl();

        let cluster_cache = Arc::new(ClusterCache::new(control_client.clone(), ttl));
        let session_resolver = Arc::new(SessionResolver::new(control_client, ttl));

        info!(
            "[GATEWAY] initialized, control={}, namespace={}",
            config.control_plane_url, config.namespace
        );

        Self {
            route_table: build_route_table(),
            config,
            cluster_cache,
            session_resolver,
        }
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
    ) -> Option<String> {
        match &route.source {
            IdentifierSource::Body => {
                let body = body?;
                IdentifierExtractor::from_body(body, route.identifier_field)
            }
            IdentifierSource::Path(param_name) => {
                IdentifierExtractor::from_path_params(path_params, param_name)
            }
            IdentifierSource::Session => {
                let session_id = path.split('/').next_back()?;
                let info = self.session_resolver.resolve(session_id).await.ok()?;
                Some(info.identifier)
            }
        }
    }

    /// 根据 identifier 和 service_type 构建 K8s Service FQDN
    ///
    /// FQDN 格式：`{service_type}-{identifier}-svc.{namespace}.svc.cluster.local`
    /// 例如：`web-agent-runner-project-123-svc.default.svc.cluster.local`
    ///
    /// 使用 ServiceType 的 Display trait 获取字符串前缀
    fn build_service_fqdn(&self, identifier: &str, service_type: ServiceType) -> String {
        format!(
            "{}-{}-svc.{}.svc.cluster.local",
            service_type, identifier, self.config.namespace
        )
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
                    Some(id) => id,
                    None => {
                        warn!(
                            "[GATEWAY] failed to extract identifier from {} ({})",
                            path, route.identifier_field
                        );
                        ctx.target = RouteTarget::ControlPlane;
                        return Ok(false);
                    }
                };

                // 获取或确保 backend cluster
                // 只读路由（GET status、SSE progress）仅查缓存，不触发 pod 创建
                let ensure_result = if route.read_only {
                    self.cluster_cache.get_only(&identifier).await
                } else {
                    self.cluster_cache
                        .get_or_ensure(&identifier, route.service_type.clone())
                        .await
                };

                match ensure_result {
                    Ok(_cluster_name) => {
                        // 直接构建 K8s Service FQDN，路由到 agent_runner
                        let fqdn = self.build_service_fqdn(&identifier, route.service_type.clone());
                        debug!("[GATEWAY] {} → {} → agent_svc ({})", path, identifier, fqdn);
                        ctx.target = RouteTarget::AgentService(fqdn);
                    }
                    Err(e) => {
                        error!(
                            "[GATEWAY] cluster cache ensure failed for {}: {}, falling back to control plane",
                            identifier, e
                        );
                        ctx.target = RouteTarget::ControlPlane;
                    }
                }
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
