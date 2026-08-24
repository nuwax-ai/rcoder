//! nuwax-file-server 前置分流反向代理（60000 端口，阶段三终态）。
//!
//! 架构位置：Java/外部 → `:60000` 本代理 → 按业务域分流：
//! - `x-service-type: userapp`（契约见 `shared_types::userapp::forward_contract`，
//!   经 shared_types 根 re-export）→ rcoder 主服务（8086）
//! - 其余 → TS nuwax-file-server（内部端口 60001）
//!
//! 判据是 header 而非路径：TS 与 Rust file-server 是同一套 REST 路径集的
//! 两种实现（路径无法区分业务域），由调用方在 header 上显式声明业务归属。
//! **header 契约**（给 Java 同事）：凡走 60000 入口的 userApp 业务请求，
//! 一律携带 `x-service-type: userapp`（与 8086 拦截层/computer handler 同一
//! 契约常量）；userApp 独有接口不经此代理，直连 8086。
//!
//! 独立 crate 而不入 rcoder-proxy：rcoder-proxy 是端口参数化容器反代
//! （matchit 路由 / API key / 容器解析），本模块只做单一职责的业务域分流；
//! 后续 TS→Rust 存量域灰度切流在此演进，不污染 rcoder-proxy。
//!
//! 上线形态：容器内 TS nuwax-file-server 退到 60001（start-services.sh），
//! 本代理由 rcoder 进程携带（config.yml `file_server_proxy:` 段存在即启动；
//! 本地 dev 配置无此段则完全不监听 60000）。

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tracing::{error, info};

pub use shared_types::{SERVICE_TYPE_HEADER, SERVICE_TYPE_USERAPP};

/// 分流代理配置（config.yml 顶层 `file_server_proxy:` 段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileServerProxyConfig {
    /// 对外监听端口（Java/外部入口；K8s NodePort 30779 → 此端口）
    pub listen_port: u16,
    /// rcoder 主服务端口（userApp 业务上游）
    pub rust_upstream_port: u16,
    /// TS nuwax-file-server 内部端口（存量域上游）
    pub ts_upstream_port: u16,
}

impl Default for FileServerProxyConfig {
    fn default() -> Self {
        Self {
            listen_port: 60000,
            rust_upstream_port: 8086,
            ts_upstream_port: 60001,
        }
    }
}

/// 业务域分流的选中上游。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upstream {
    /// rcoder 主服务（userApp 业务）
    Rust(u16),
    /// TS nuwax-file-server（存量域）
    Ts(u16),
}

/// userApp 业务判定：`x-service-type` 值为 `userapp`（wire 值小写，严格相等）。
fn is_userapp_request(header_value: Option<&str>) -> bool {
    header_value.is_some_and(|v| v == SERVICE_TYPE_USERAPP)
}

impl FileServerProxyConfig {
    /// 分流规则纯函数：`x-service-type: userapp` → Rust 上游，其余 → TS 上游。
    pub fn upstream_port_for(&self, service_type_header: Option<&str>) -> Upstream {
        if is_userapp_request(service_type_header) {
            Upstream::Rust(self.rust_upstream_port)
        } else {
            Upstream::Ts(self.ts_upstream_port)
        }
    }
}

/// 启动分流反向代理。
///
/// 阻塞至 shutdown 信号（sender drop 或显式发送）后返回；pingora 线程
/// `run_forever` 不随返回退出，由进程退出时 OS 清理（与 rcoder-proxy 同模式）。
/// 监听端口被占用时 fail fast 返回 `Err`（端口空置问题不可静默）。
pub async fn run_file_server_proxy(
    config: FileServerProxyConfig,
    shutdown_rx: oneshot::Receiver<()>,
) -> Result<(), String> {
    info!(
        listen_port = config.listen_port,
        rust_upstream = format!(
            "127.0.0.1:{} ({SERVICE_TYPE_HEADER}: {SERVICE_TYPE_USERAPP})",
            config.rust_upstream_port
        ),
        ts_upstream = format!(
            "127.0.0.1:{} (其余全部, 含 computer/project/git/build/health)",
            config.ts_upstream_port
        ),
        "file-server 分流代理启动中"
    );

    // bind 预检 fail fast：pingora 真正 bind 在 run_forever 线程内，
    // 失败只进线程日志不易察觉（60000 空置会直接打断 Java 链路）
    {
        let probe = std::net::TcpListener::bind(("0.0.0.0", config.listen_port)).map_err(|e| {
            error!(
                "listen 0.0.0.0:{} 预检失败（端口被占用?）: {e}",
                config.listen_port
            );
            format!(
                "listen 0.0.0.0:{} 预检失败（端口被占用?）: {e}",
                config.listen_port
            )
        })?;
        drop(probe);
    }

    let opt = pingora_core::server::configuration::Opt::default();
    let mut server = pingora_core::server::Server::new(Some(opt))
        .map_err(|e| format!("create pingora server: {e}"))?;
    server.bootstrap();

    let mut proxy = pingora_proxy::http_proxy_service(
        &server.configuration,
        SplitProxy {
            config: config.clone(),
        },
    );
    proxy.add_tcp(&format!("0.0.0.0:{}", config.listen_port));
    server.add_service(proxy);

    std::thread::Builder::new()
        .name("file-server-proxy".to_string())
        .spawn(move || {
            info!(
                "file-server 分流代理运行中 (0.0.0.0:{})",
                config.listen_port
            );
            server.run_forever();
        })
        .map_err(|e| format!("spawn file-server-proxy 线程失败: {e}"))?;

    let _ = shutdown_rx.await;
    info!("file-server 分流代理收到关闭信号，pingora 线程由 OS 清理");
    Ok(())
}

/// 业务域分流代理（轻量 ProxyHttp：只选上游，不改写请求/响应）。
struct SplitProxy {
    config: FileServerProxyConfig,
}

#[async_trait::async_trait]
impl pingora_proxy::ProxyHttp for SplitProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        session: &mut pingora_proxy::Session,
        _ctx: &mut Self::CTX,
    ) -> pingora_core::Result<Box<pingora_core::upstreams::peer::HttpPeer>> {
        let service_type = session
            .req_header()
            .headers
            .get(SERVICE_TYPE_HEADER)
            .and_then(|v| v.to_str().ok());
        let port = match self.config.upstream_port_for(service_type) {
            Upstream::Rust(port) => port,
            Upstream::Ts(port) => port,
        };
        Ok(Box::new(pingora_core::upstreams::peer::HttpPeer::new(
            ("127.0.0.1", port),
            false,
            String::new(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> FileServerProxyConfig {
        FileServerProxyConfig::default()
    }

    #[test]
    fn userapp_service_type_routes_to_rust() {
        let c = cfg();
        // 同一路径集下, header 决定业务归属
        for path in [
            "/api/computer/create-workspace",
            "/api/project/list",
            "/health",
        ] {
            assert_eq!(
                c.upstream_port_for(Some("userapp")),
                Upstream::Rust(8086),
                "{path} 带 {SERVICE_TYPE_HEADER}: userapp 应走 Rust"
            );
        }
    }

    #[test]
    fn missing_or_other_service_type_routes_to_ts() {
        let c = cfg();
        for path in [
            "/health",
            "/api/version",
            "/api/computer/create-workspace",
            "/api/project/list",
            "/api/git/status",
            "/api/build/start",
            "/",
        ] {
            assert_eq!(c.upstream_port_for(None), Upstream::Ts(60001), "{path}");
        }
        // 非本业务域声明（computer 等）与非法值一律 TS——契约违规 404 可见而非静默误路由
        assert_eq!(c.upstream_port_for(Some("computer")), Upstream::Ts(60001));
        assert_eq!(c.upstream_port_for(Some("UserApp")), Upstream::Ts(60001));
        assert_eq!(c.upstream_port_for(Some("")), Upstream::Ts(60001));
    }

    #[test]
    fn custom_ports_respected() {
        let c = FileServerProxyConfig {
            listen_port: 61000,
            rust_upstream_port: 18086,
            ts_upstream_port: 6001,
        };
        assert_eq!(c.upstream_port_for(Some("userapp")), Upstream::Rust(18086));
        assert_eq!(c.upstream_port_for(None), Upstream::Ts(6001));
    }
}
