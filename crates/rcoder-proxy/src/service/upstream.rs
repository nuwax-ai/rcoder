//! deploy-host 拨号统一出口（Pingora 数据面）。
//!
//! 宿主机形态下各 handler 拿到的 host 是 published-port 注册表键
//! （container_name——来自 ContainerLookup/vnc_backends/app_backends 注册），
//! 实际拨号一律经 [`shared_types::published`] 解析 `127.0.0.1:{host_port}`；
//! 容器/集群内形态原样返回（容器 IP / FQDN 可达）。全部 HttpPeer 构造点
//! 统一经 [`dial_peer`]/[`dial_addr`]，新增拨号点不得直拼。

/// 元组形式（`HttpPeer::new((host, port), ..)`）。
pub fn dial_peer(host: &str, port: u16) -> (String, u16) {
    #[cfg(feature = "deploy-host")]
    if shared_types::is_deploy_host() {
        let addr = shared_types::published::resolve_published_addr(host, port);
        return (addr.ip().to_string(), addr.port());
    }
    (host.to_string(), port)
}

/// 字符串形式（`"host:port"`，如 ctx.upstream_host / HttpPeer::new(addr_str)）。
pub fn dial_addr(host: &str, port: u16) -> String {
    #[cfg(feature = "deploy-host")]
    if shared_types::is_deploy_host() {
        return shared_types::published::resolve_published_addr(host, port).to_string();
    }
    format!("{host}:{port}")
}
