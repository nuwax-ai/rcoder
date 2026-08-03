//! API 密钥代理处理函数
//!
//! 处理 `/api/{service_name}/{*path}` 路径的 API 密钥代理。
//! 功能：注入真实 API 密钥，重写 URI 到真实 API 端点。

use dashmap::DashMap;
use matchit::Params;
use pingora_core::Result as PingoraResult;
use pingora_core::protocols::TcpKeepalive;
use pingora_core::upstreams::peer::{ALPN, HttpPeer};
use pingora_http::RequestHeader;
use shared_types::ModelProviderConfig;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::service::types::{ProxyMetrics, TrackingCtx};
use crate::service::utils;

/// 处理 API 密钥代理请求
///
/// 路径格式: `/api/{service_name}/{*path}`
/// 例如: `/api/anthropic/v1/messages`
///
/// 安全机制：
/// 1. 从 ApiKeyManager 读取真实 API 密钥配置
/// 2. 移除客户端传入的占位密钥
/// 3. 注入真实 API 密钥到请求头
/// 4. 重写 URI 到真实 API 端点
pub async fn handle_api_proxy_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
    ctx: &mut TrackingCtx,
    api_key_manager: &Arc<DashMap<String, ModelProviderConfig>>,
) -> PingoraResult<()> {
    // 1. 提取服务名称（如 "anthropic", "openai"）
    let service_name = params.get("service_name").ok_or_else(|| {
        error!("API proxy route missing service_name param");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    // 2. 提取 API 路径（如 "v1/messages"）
    let api_path = params.get("path").unwrap_or("");

    // 记录服务名到 ctx（用于错误响应体日志）
    ctx.api_service_name = Some(service_name.to_string());

    debug!("API proxy request: service_name={}", service_name);

    // [DEBUG] 打印原始请求的所有 headers
    {
        let method = upstream_request.method.as_str();
        debug!(
            "[API_PROXY_DEBUG] ====== Original request ======\n  Method: {}\n  Path: {}",
            method,
            original_uri.path()
        );
        for (name, value) in upstream_request.headers.iter() {
            let val_str = value.to_str().unwrap_or("<binary>");
            // 对敏感 header 做脱敏
            if name.as_str().eq_ignore_ascii_case("x-api-key")
                || name.as_str().eq_ignore_ascii_case("authorization")
            {
                let masked = utils::mask_header_value(val_str);
                debug!("[API_PROXY_DEBUG]   Header: {} = {}", name, masked);
            } else {
                debug!("[API_PROXY_DEBUG]   Header: {} = {}", name, val_str);
            }
        }
    }

    // 3. 从 ApiKeyManager 查询 API 密钥配置
    let api_config = api_key_manager.get(service_name).ok_or_else(|| {
        // 只记录数量不暴露 service 名列表，避免配置结构泄漏到日志
        let count = api_key_manager.iter().count();
        warn!(
            "[API_PROXY] Cannot find API key config for service '{}' (configured: {} services)",
            service_name, count
        );
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404)).more_context(format!(
            "Cannot find API key config for service {}, please ensure it is properly configured",
            service_name
        ))
    })?;

    let config = api_config.value();
    let base_url = config.base_url.trim_end_matches('/');

    // 仅记录 origin，避免 base_url 的用户信息、租户路径或 query 参数进入日志。
    let base_url_origin = utils::url_origin_for_log(base_url);
    debug!(
        "[API_PROXY_DEBUG] ====== DashMap config (service={}) ======\n  base_url: {}\n  api_protocol: {:?}\n  requires_openai_auth: {}\n  api_key: {}",
        service_name,
        base_url_origin,
        config.api_protocol,
        config.requires_openai_auth,
        utils::mask_header_value(&config.api_key),
    );

    // 4. 移除客户端传入的占位密钥（安全措施）
    upstream_request.remove_header("x-api-key");
    upstream_request.remove_header("authorization");
    upstream_request.remove_header("x-api-version"); // 移除可能的版本标识

    // 5. 注入真实 API 密钥
    // Anthropic 协议使用 x-api-key，OpenAI 协议使用 Authorization: Bearer
    // 优先根据 api_protocol 判断，而不是 requires_openai_auth

    // 判断使用哪种认证格式
    let use_anthropic_auth = config
        .api_protocol
        .as_ref()
        .map(|p| {
            let protocol = p.to_lowercase();
            protocol != "openai" // 不是 openai 就用 Anthropic 格式
        })
        .unwrap_or(!config.requires_openai_auth);

    if use_anthropic_auth {
        upstream_request.insert_header("x-api-key", &config.api_key)?;
        info!(
            "[API_PROXY] Injected Anthropic format x-api-key: {} (api_protocol={:?})",
            utils::mask_header_value(&config.api_key),
            config.api_protocol
        );
    } else {
        upstream_request.insert_header("authorization", format!("Bearer {}", config.api_key))?;
        info!(
            "[API_PROXY] Injected OpenAI format Bearer: {} (api_protocol={:?})",
            utils::mask_header_value(&config.api_key),
            config.api_protocol
        );
    }

    // 6. 重写 URI 到真实 API 端点
    let new_uri_str = if api_path.is_empty() {
        format!("{}/", base_url)
    } else {
        format!("{}/{}", base_url, api_path)
    };

    // 保留查询参数
    let new_uri_str = if let Some(query) = original_uri.query() {
        format!("{}?{}", new_uri_str, query)
    } else {
        new_uri_str
    };

    debug!("[API_PROXY_DEBUG] proxy origin: {}", base_url_origin);

    let new_uri = new_uri_str.parse::<http::Uri>().map_err(|e| {
        error!("API proxy URI rewrite failed: {}", e);
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    upstream_request.set_uri(new_uri);

    // 8. 设置 Host 头（从 base_url 提取）
    if let Some(host) = base_url
        .strip_prefix("https://")
        .or_else(|| base_url.strip_prefix("http://"))
        .and_then(|s: &str| s.split('/').next())
    {
        upstream_request.insert_header("Host", host)?;
        debug!("Host header already set: {}", host);
    }

    // 9. 设置通用代理头
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-API-Proxy", "pingora-proxy")?;
    upstream_request.insert_header("X-Service-Name", service_name)?;

    info!(
        "[API_PROXY] {} request rewritten to origin: {}",
        service_name, base_url_origin
    );

    // [DEBUG] 打印最终发送到上游的所有 headers
    {
        debug!("[API_PROXY_DEBUG] ====== request Headers ======");
        for (name, value) in upstream_request.headers.iter() {
            let val_str = value.to_str().unwrap_or("<binary>");
            if name.as_str().eq_ignore_ascii_case("x-api-key")
                || name.as_str().eq_ignore_ascii_case("authorization")
            {
                let masked = utils::mask_header_value(val_str);
                debug!("[API_PROXY_DEBUG]   {} = {}", name, masked);
            } else {
                debug!("[API_PROXY_DEBUG]   {} = {}", name, val_str);
            }
        }
        debug!("[API_PROXY_DEBUG] ====== response Headers ======");
    }

    Ok(())
}

/// 处理 API 密钥代理的上游选择
///
/// 返回真实 API 端点的 HttpPeer
pub async fn handle_api_proxy_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    api_key_manager: &Arc<DashMap<String, ModelProviderConfig>>,
    metrics: &Arc<ProxyMetrics>,
) -> PingoraResult<Box<HttpPeer>> {
    // 1. 提取服务名称
    let service_name = params.get("service_name").ok_or_else(|| {
        error!("API proxy route missing service_name param");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    // 2. 从 ApiKeyManager 查询 API 配置
    let api_config = api_key_manager.get(service_name).ok_or_else(|| {
        warn!("{} not in API config", service_name);
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(404)).more_context(format!(
            "API key config for service {} not found",
            service_name
        ))
    })?;

    let config = api_config.value();
    let base_url = &config.base_url;

    // 3. 解析真实 API 端点的 host 和 port
    // 支持 https://api.anthropic.com 和 https://api.openai.com:443 格式
    let (host, port, use_tls) = if let Some(https_url) = base_url.strip_prefix("https://") {
        let host_part = https_url.split('/').next().unwrap_or(https_url);
        if let Some(port_str) = host_part.split(':').nth(1) {
            let port = port_str.parse::<u16>().unwrap_or(443);
            let host = host_part.split(':').next().unwrap_or(host_part);
            (host, port, true)
        } else {
            (host_part, 443, true)
        }
    } else if let Some(http_url) = base_url.strip_prefix("http://") {
        let host_part = http_url.split('/').next().unwrap_or(http_url);
        if let Some(port_str) = host_part.split(':').nth(1) {
            let port = port_str.parse::<u16>().unwrap_or(80);
            let host = host_part.split(':').next().unwrap_or(host_part);
            (host, port, false)
        } else {
            (host_part, 80, false)
        }
    } else {
        return Err(
            pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
                .more_context(format!("invalid base_url format: {}", base_url)),
        );
    };

    // 4. 记录指标
    metrics.record_request();
    metrics.inc_active();

    // 4.1 记录上游信息到 ctx（用于 response_filter 打印协议）
    ctx.upstream_host = Some(format!("{}:{}", host, port));
    ctx.use_tls = use_tls;

    // 5. 创建真实 API 端点的 HttpPeer
    // 注意：SNI 必须设置为目标主机名，否则 TLS 握手会失败
    // 同时需要启用 HTTP/2 支持，因为很多 API 服务（如 open.bigmodel.cn）强制使用 HTTP/2
    let mut peer = HttpPeer::new(
        (host, port),
        use_tls,          // 根据协议决定是否使用 TLS
        host.to_string(), // SNI 必须设置为目标主机名
    );
    // 启用 HTTP/2 支持，优先 H2，兼容 H1
    peer.options.alpn = ALPN::H2H1;

    // 5.1 打印代理连接信息（在 ALPN 设置之后，确保日志准确性）
    let alpn_str = match peer.options.alpn {
        ALPN::H2 => "H2",
        ALPN::H2H1 => "H2H1",
        ALPN::H1 => "H1",
        ALPN::Custom(_) => "Custom",
    };
    info!(
        "[API_PROXY] {} -> {}:{} (TLS: {}, ALPN: {})",
        service_name,
        utils::mask_domain(host),
        port,
        use_tls,
        alpn_str
    );

    // 上游连接健康检测配置
    // HTTP/2 PING 心跳: 每 30 秒发送 PING 帧检测上游连接健康
    peer.options.h2_ping_interval = Some(Duration::from_secs(30));
    // TCP Keepalive: 操作系统级别的连接保活，适用于 HTTP/1.1 后备
    peer.options.tcp_keepalive = Some(TcpKeepalive {
        idle: Duration::from_secs(60),    // 60 秒无数据后开始探测
        interval: Duration::from_secs(5), // 每 5 秒探测一次
        count: 5,                         // 5 次失败后认为断开
        #[cfg(target_os = "linux")]
        user_timeout: Duration::from_secs(85), // Linux: 数据未确认的最大时间
    });
    // 连接超时配置
    peer.options.connection_timeout = Some(Duration::from_secs(10)); // 连接建立超时
    peer.options.total_connection_timeout = Some(Duration::from_secs(30)); // 含 TLS 握手的总超时
    // read_timeout: 不设置，默认 None，适合 AI API 长时间推理
    peer.options.idle_timeout = Some(Duration::from_secs(90)); // 连接池空闲超时

    let peer = Box::new(peer);

    Ok(peer)
}
