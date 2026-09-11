//! userApp 生产应用流量代理（免端口）
//!
//! 处理 `/api/v1/userapp/proxy/app/prod/{user_id}/{app_id}/{*path}` 路径的反向代理：
//! **主路径确定性命名动态解析**（`find_app_runtime_addr`：K8s = svc FQDN /
//! Docker = 容器名 DNS，与 dbx prod 族同款）——无状态，多副本任一 pod 的
//! pingora 均可服务（注册表内存态导致非受理副本 502 的架构债根治）。
//!
//! 回退：`app_backends` 注册表（app_manager 部署时注册，仅受理副本持有）——
//! lookup 未注入或该 app 恰只有自定义端口注册时使用（防御直接 REST create
//! 声明非 9080 端口的 app；release 流程恒 pin 9080，正常路径不走此分支）。
//! user_id 不参与后端解析，仅日志/归属锚点。

use dashmap::DashMap;
use matchit::Params;
use pingora_core::Result as PingoraResult;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error};

use crate::service::types::{ProxyMetrics, TrackingCtx};
use crate::service::utils;

/// 处理生产应用流量代理请求
///
/// 路径格式: `/api/v1/userapp/proxy/app/prod/{user_id}/{app_id}/{*path}` —— 提取
/// app_id，重写 URI 去掉前缀（免端口：上游端口在 upstream 阶段解析），设置代理标识头。
pub async fn handle_prod_app_request(
    upstream_request: &mut RequestHeader,
    original_uri: &http::Uri,
    params: Params<'_, '_>,
) -> PingoraResult<()> {
    let user_id = params.get("user_id").unwrap_or("");
    let app_id = params.get("app_id").ok_or_else(|| {
        error!("prod app proxy route missing app_id params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    // 从原始 URI 提取剩余路径（strip /api/v1/userapp/proxy/app/prod/{user_id}/{app_id}，保留尾斜杠）
    let original_path = original_uri.path();
    let prefix = format!("/api/v1/userapp/proxy/app/prod/{user_id}/{app_id}");
    let target_path = if original_path.len() <= prefix.len() {
        "/".to_string()
    } else {
        original_path[prefix.len()..].to_string()
    };

    debug!(
        "prod app proxy request: user_id={}, app_id={}, target_path={}",
        user_id, app_id, target_path
    );

    upstream_request.insert_header("Host", "127.0.0.1")?;
    let new_uri = utils::rewrite_uri(original_uri, target_path)?;
    upstream_request.set_uri(new_uri);
    utils::set_common_headers(upstream_request)?;
    upstream_request.insert_header("X-Port-Proxy", "pingora-userapp-prod")?;
    Ok(())
}

/// 处理生产应用流量代理的上游连接选择
///
/// 主路径：`find_app_runtime_addr` 确定性命名动态解析（无状态，多副本任一
/// pod 可服务）+ pingap 统一入口 `APP_ENTRY_PORT`；回退：`app_backends`
/// 注册表（lookup 未注入，或该 app 恰只有一个自定义端口注册时用之）；
/// 再未命中 → 502（Fail Fast——app 专用路径不能像通用 /proxy/{port} 那样
/// 猜 host，否则会路由到错误 app）。
pub async fn handle_prod_app_upstream(
    ctx: &mut TrackingCtx,
    params: Params<'_, '_>,
    app_backends: &Arc<DashMap<(String, u16), String>>,
    metrics: &Arc<ProxyMetrics>,
    container_lookup: &Option<Arc<dyn shared_types::ContainerLookup>>,
) -> PingoraResult<Box<HttpPeer>> {
    let app_id = params.get("app_id").ok_or_else(|| {
        error!("prod app proxy upstream missing app_id params");
        pingora_core::Error::new(pingora_core::ErrorType::HTTPStatus(400))
    })?;

    metrics.record_request();

    // 主路径：确定性命名（K8s svc FQDN / Docker 容器名）——不依赖受理副本
    // 的内存注册表；端口优先取注册表中该 app 的已注册端口（兼容 REST create
    // 自定义端口场景，release 流程恒 9080），无注册记录时默认 pingap 入口。
    // 解析为具体地址（IPv4 优先、IPv6 兜底）——Docker 网络容器名解析常返回
    // v6 优先（fd07::），而应用容器普遍仅监听 0.0.0.0（v4），v6 目标连不上
    // v4 监听；名字解析失败则落入注册表回退链。
    let resolved_addr = 'addr: {
        // 回退分支用的注册值历史上是 IP 字面量（IPv4），按字面量同步解析即可
        let try_parse = |host: &str, port: u16| -> Option<std::net::SocketAddr> {
            use std::net::ToSocketAddrs;
            (host, port).to_socket_addrs().ok()?.next()
        };
        if let Some(host) = container_lookup
            .as_ref()
            .and_then(|lookup| lookup.find_app_runtime_addr(app_id))
            .filter(|h| !h.is_empty())
        {
            let port = app_backends
                .iter()
                .find(|e| e.key().0 == app_id)
                .map(|e| e.key().1)
                .unwrap_or(shared_types::APP_ENTRY_PORT);
            if let Some(addr) = resolve_preferring_ipv4(&host, Some(port)).await
                && !is_dns_fake_ip(&addr)
            {
                break 'addr addr;
            }
            debug!(
                "prod app dynamic naming unresolved or unreachable, falling back to registry: app_id={}, host={host}",
                app_id
            );
        }
        if let Some(host) = app_backends.get(&(app_id.to_string(), shared_types::APP_ENTRY_PORT)) {
            // 回退一：lookup 未注入/动态名解析失败——注册表直查（受理副本内存态，
            // 注册值历史上是 IPv4 字面量，直接拼端口解析）
            if let Some(addr) = try_parse(host.value(), shared_types::APP_ENTRY_PORT) {
                break 'addr addr;
            }
        }
        // 回退二：该 app 恰只有一个已注册端口（自定义端口防御分支）
        let mut candidates = app_backends.iter().filter(|e| e.key().0 == app_id);
        if let (Some(e), None) = (candidates.next(), candidates.next()) {
            let port = e.key().1;
            debug!(
                "prod app fallback to sole registered port: app_id={}, port={}",
                app_id, port
            );
            if let Some(addr) = try_parse(e.value(), port) {
                break 'addr addr;
            }
        }
        error!(
            "prod app backend unresolvable: app_id={} (dynamic naming unresolved, not registered)",
            app_id
        );
        return Err(pingora_core::Error::new(
            pingora_core::ErrorType::HTTPStatus(502),
        ));
    };

    ctx.target_port = Some(resolved_addr.port());
    metrics.record_request_port(resolved_addr.port());
    // inc_active 放在解析成功后（对齐 dev_app_proxy：502 不进 response_filter，
    // 提前 inc 会造成 gauge 单调虚增）
    metrics.inc_active();

    debug!("prod app route: app_id={}, {}", app_id, resolved_addr);

    // 创建 HTTP Peer（长连接配置，支持 WebSocket / HMR）
    let mut peer = HttpPeer::new(resolved_addr, false, "".to_string());
    peer.options.connection_timeout = Some(Duration::from_secs(10));
    peer.options.read_timeout = None;
    peer.options.write_timeout = None;
    peer.options.total_connection_timeout = Some(Duration::from_secs(15));
    peer.options.idle_timeout = Some(Duration::from_secs(3600));

    Ok(Box::new(peer))
}

/// 主机名/IP 解析为具体 SocketAddr，**IPv4 优先、IPv6 兜底**（双栈支持）。
///
/// Docker 网络（OrbStack 等）的容器名 DNS 解析常返回 v6 优先（fd07:: ULA），
/// 而应用容器普遍仅监听 `0.0.0.0`（v4）——v6 目标连不上 v4 监听；v4 优先
/// 规避该错配，v6-only 环境仍可用。IP 字面量（v4/v6）parse 直接构造零 DNS。
/// `port` 为 None 时仅解析地址（端口置 0，由调用方改写）。
async fn resolve_preferring_ipv4(host: &str, port: Option<u16>) -> Option<std::net::SocketAddr> {
    // IP 字面量（含 v6）：parse 判定，零 DNS 开销
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Some(std::net::SocketAddr::new(ip, port.unwrap_or(0)));
    }
    // 名字：异步解析（hot path 不阻塞 worker），结果集 IPv4 优先
    let addrs: Vec<_> = tokio::net::lookup_host((host, port.unwrap_or(0)))
        .await
        .ok()?
        .collect();
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.first())
        .map(|a| {
            let mut addr = *a;
            if let Some(p) = port {
                addr.set_port(p);
            }
            addr
        })
}

/// 是否 DNS 合成假 IP（RFC 2544 benchmark 段 198.18.0.0/15）。
///
/// OrbStack 等环境的内置 DNS 对**不存在**的容器名返回 fake-ip 而非
/// NXDOMAIN，且连接层的 TCP 握手也被劫持应答（探测无法甄别，随后数据
/// 黑洞）——正常业务网络不会使用该保留段，命中即视为解析失败。
fn is_dns_fake_ip(addr: &std::net::SocketAddr) -> bool {
    if let std::net::IpAddr::V4(ip) = addr.ip() {
        let o = ip.octets();
        o[0] == 198 && (o[1] & 0xFE) == 18 // 198.18.0.0/15（198.18.x 与 198.19.x）
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> std::net::SocketAddr {
        format!("{s}:9080").parse().unwrap()
    }

    /// RFC 2544 benchmark 段精确命中：198.18.x 与 198.19.x（/15 的两个 /24 块），
    /// 边界外（198.17/198.20/其他段）与 v6 不命中。
    #[test]
    fn is_dns_fake_ip_matches_benchmark_range_only() {
        for hit in ["198.18.0.1", "198.18.17.89", "198.19.255.254"] {
            assert!(is_dns_fake_ip(&v4(hit)), "{hit} 应命中 fake-ip 段");
        }
        for miss in [
            "198.17.255.255",
            "198.20.0.1",
            "192.168.97.9",
            "10.42.0.221",
        ] {
            assert!(!is_dns_fake_ip(&v4(miss)), "{miss} 不应命中");
        }
        let v6: std::net::SocketAddr = "[fd07:b51a::9]:9080".parse().unwrap();
        assert!(!is_dns_fake_ip(&v6), "v6 不参与 fake-ip 判定");
    }

    /// 字面量直返（v4/v6 家族保留、端口透传/缺省 0），不依赖 DNS。
    #[tokio::test]
    async fn resolve_preferring_ipv4_literal_passthrough() {
        let a = resolve_preferring_ipv4("192.168.97.9", Some(9080)).await;
        assert_eq!(
            a,
            Some("192.168.97.9:9080".parse::<std::net::SocketAddr>().unwrap())
        );
        let b = resolve_preferring_ipv4("fd07::1", None).await;
        assert_eq!(b.map(|x| x.port()), Some(0));
    }
}
