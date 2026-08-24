//! nuwax-file-server 前置分流反向代理（60000 端口，阶段三终态）。
//!
//! 架构位置：Java/外部 → `:60000` 本代理 → 按业务域分流：
//! - `/api/userapp*` 路径前缀，**或** `x-service-type: userapp` header（契约见
//!   `shared_types::userapp::forward_contract`，经 shared_types 根 re-export）
//!   → rcoder 主服务（8086）
//! - 其余 → TS nuwax-file-server（内部端口 60001）
//!
//! 双判据的由来：`/api/userapp` 前缀是 userApp 新契约的专属路径（TS 无此路由，
//! 按 path 分流零歧义）——Java 同事尚未接入 header 契约时的兜底判据；
//! `x-service-type` 是存量路径（computer/project 等两实现同构）上的业务域显式
//! 声明——Java 同事接入后的正名路径。两者任一命中即走 Rust 上游。
//!
//! **header 契约**（待传达给 Java 同事）：
//! - 走 60000 入口的 userApp 业务请求（含存量路径形态）带 `x-service-type: userapp`
//! - 直连 8086 的 `/api/userapp/*` 请求带 `x-app-id: {app_id}`（转发定位容器；
//!   POST/multipart 的 app_id 不解析 body 拿不到，header 是唯一无损通道）
//!
//! 独立 crate 而不入 rcoder-proxy：rcoder-proxy 是端口参数化容器反代，本模块
//! 只做单一职责的业务域分流；后续 TS→Rust 存量域灰度切流在此演进。
//!
//! ## 运行时生命周期
//!
//! `rcoder file-server {start,stop,restart,status}` CLI 经 rcoder admin API
//! `/api/system/file-server/*` 驱动（模式复刻自阶段二内嵌 file-server 的运行时启停，
//! f55f230）。开发测试期在 60000 入口切换"分流代理 vs TS 直跑"对比两侧实现：
//! 1. `rcoder file-server stop` —— 60000 释放
//! 2. 容器内 `nuwax-file-server start --env production --port 60000`（TS 直跑）
//! 3. 对比完成后 kill TS，`rcoder file-server start` 代理重占
//!
//! 实现用 hyper（listener 自持 + graceful shutdown）而非 pingora：pingora
//! `Server::run_forever` 无程序化停机 API，无法支撑反复启停释放端口。
//! 启动语义：同步 bind（返回时状态准确）；stop 返回时端口已确认释放。

use std::sync::OnceLock;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

pub use shared_types::{SERVICE_TYPE_HEADER, SERVICE_TYPE_USERAPP};

/// userApp 业务路由前缀（header 未接入期的兜底判据）。
pub const USERAPP_PATH_PREFIX: &str = "/api/userapp";

/// 统一响应 body（上游 Incoming 与本地错误 Full 的归一）。
type ProxyBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

/// 上游 HTTP 客户端（连接池复用）。
type ProxyClient = hyper_util::client::legacy::Client<
    hyper_util::client::legacy::connect::HttpConnector,
    Incoming,
>;

/// 分流代理配置（config.yml 顶层 `file_server_proxy:` 段 / agent_runner env 构造）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileServerProxyConfig {
    /// 对外监听端口（Java/外部入口；K8s NodePort 30779 → 此端口）
    pub listen_port: u16,
    /// rcoder 主服务端口（userApp 业务上游；容器形态=内嵌 Rust file-server 端口）
    pub rust_upstream_port: u16,
    /// TS nuwax-file-server 内部端口（存量域上游；容器形态为复用面预留，未使用）
    pub ts_upstream_port: u16,
    /// 路由策略（两种部署形态）
    #[serde(default)]
    pub policy: RoutePolicy,
}

impl Default for FileServerProxyConfig {
    fn default() -> Self {
        Self {
            listen_port: 60000,
            rust_upstream_port: 8086,
            ts_upstream_port: 60001,
            policy: RoutePolicy::default(),
        }
    }
}

/// 路由策略——同一 crate 服务两种部署形态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoutePolicy {
    /// rcoder 主 pod 形态：userApp 判据（path 前缀或 header）→ rust 上游，
    /// 其余 → ts 上游（存量域继续 TS nuwax-file-server）
    #[default]
    UserappSplit,
    /// agent-runner 容器形态：一律 rust 上游（现状行为等价——容器内 60000 进来
    /// 的全部进内嵌 Rust file-server；ts 上游为复用面决策后的切换预留）
    AllRust,
}

/// 业务域分流的选中上游。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upstream {
    /// rcoder 主服务（userApp 业务）
    Rust(u16),
    /// TS nuwax-file-server（存量域）
    Ts(u16),
}

/// userApp 路径前缀的段边界形式（`/api/userapplication` 不误命中）。
const USERAPP_PATH_PREFIX_SLASH: &str = "/api/userapp/";

/// userApp 业务判定的 path 判据：`/api/userapp` 精确或 `/api/userapp/*`。
/// TS 无此路由（走 TS 也 404），按前缀分流零歧义——Java 同事加
/// `x-service-type` header 前的兜底判据，header 是未来的正名路径。
fn is_userapp_path(path: &str) -> bool {
    path == USERAPP_PATH_PREFIX || path.starts_with(USERAPP_PATH_PREFIX_SLASH)
}

/// userApp 业务判定的 header 判据：`x-service-type` 值为 `userapp`
/// （wire 值小写，严格相等）。
fn is_userapp_service_type(header_value: Option<&str>) -> bool {
    header_value.is_some_and(|v| v == SERVICE_TYPE_USERAPP)
}

impl FileServerProxyConfig {
    /// 分流规则纯函数（按 [`RoutePolicy`] 分派）：
    /// - [`RoutePolicy::UserappSplit`]：`/api/userapp*` 前缀或
    ///   `x-service-type: userapp` header（任一命中）→ Rust 上游，其余 → TS 上游
    /// - [`RoutePolicy::AllRust`]：一律 Rust 上游
    pub fn upstream_port_for(&self, path: &str, service_type_header: Option<&str>) -> Upstream {
        let to_rust = match self.policy {
            RoutePolicy::UserappSplit => {
                is_userapp_path(path) || is_userapp_service_type(service_type_header)
            }
            RoutePolicy::AllRust => true,
        };
        if to_rust {
            Upstream::Rust(self.rust_upstream_port)
        } else {
            Upstream::Ts(self.ts_upstream_port)
        }
    }
}

// ── 全局生命周期管理（admin API / CLI 复刻自 f55f230 的内嵌 file-server 模式）──

/// 运行中的代理实例（shutdown 信号 + serve task + 监听地址）。
struct RunningInstance {
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    address: String,
}

/// 配置注册（main 无条件调用，段缺失时用 Default——本地 dev 也可经 admin API 拉起）。
static CONFIG: OnceLock<FileServerProxyConfig> = OnceLock::new();

/// 当前实例（None = 未运行，60000 未被本代理占用）。
static INSTANCE: tokio::sync::Mutex<Option<RunningInstance>> = tokio::sync::Mutex::const_new(None);

/// 注册配置（幂等，首次生效）。
///
/// main 启动时调用；config.yml 无 `file_server_proxy` 段时传
/// [`FileServerProxyConfig::default`]（不自动启动，仅让运行时 `start` 可用）。
pub fn init(config: FileServerProxyConfig) {
    if CONFIG.set(config).is_err() {
        tracing::debug!("file-server-proxy config already registered, keep first");
    }
}

/// 当前运行状态：Some(address) = 运行中，None = 已停止。
pub async fn status() -> Option<String> {
    INSTANCE.lock().await.as_ref().map(|i| i.address.clone())
}

/// 启动分流代理（幂等）。同步 bind（而非 spawn 内 bind），返回时状态准确。
pub async fn try_start() -> Result<String, String> {
    let mut guard = INSTANCE.lock().await;
    if let Some(instance) = guard.as_ref() {
        return Ok(instance.address.clone());
    }
    let config = CONFIG.get().cloned().unwrap_or_default();

    let address = format!("0.0.0.0:{}", config.listen_port);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|e| format!("bind {address} 失败（端口被占用?）: {e}"))?;

    let client: ProxyClient =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build_http();

    let shutdown = CancellationToken::new();
    let token = shutdown.clone();
    let listening_addr = address.clone();
    let task = tokio::spawn(async move {
        info!(
            "file-server 分流代理运行中 ({listening_addr}; {USERAPP_PATH_PREFIX}* 或 \
             {SERVICE_TYPE_HEADER}: {SERVICE_TYPE_USERAPP} → 127.0.0.1:{}, 其余 → 127.0.0.1:{})",
            config.rust_upstream_port, config.ts_upstream_port
        );
        serve(listener, client, config, token).await;
        // serve 意外退出（accept 错误等）: 清理 INSTANCE, 避免 status 误报 running
        cleanup_dead_instance().await;
    });

    info!("file-server 分流代理启动: {address}");
    *guard = Some(RunningInstance {
        shutdown,
        task,
        address: address.clone(),
    });
    Ok(address)
}

/// serve task 结束后的 INSTANCE 清理（正常 stop 已 take，此处只兜底意外退出）。
async fn cleanup_dead_instance() {
    let mut guard = INSTANCE.lock().await;
    if guard.as_ref().is_some_and(|i| i.task.is_finished()) {
        guard.take();
        warn!("file-server 分流代理实例已随 serve 退出清理（意外退出）");
    }
}

/// 停止分流代理（幂等）。
///
/// cancel → 等 serve task 结束（**10s 超时 abort + 再 await**，确保 listener drop
/// 端口释放）；返回时 60000 已可用（外部服务如 TS nuwax-file-server 可立即 bind）。
pub async fn stop() -> Result<(), String> {
    let instance = INSTANCE.lock().await.take();
    let Some(mut instance) = instance else {
        return Ok(());
    };
    // 锁已随 guard drop 释放，等 task 期间不阻塞 status/start
    instance.shutdown.cancel();
    let timeout = std::time::Duration::from_secs(10);
    if tokio::time::timeout(timeout, &mut instance.task)
        .await
        .is_err()
    {
        warn!("file-server 分流代理优雅停机超时, abort");
        instance.task.abort();
        // abort 仅调度取消, 再 await 确保 listener 已 drop（端口真正释放）;
        // 随后的 JoinError 是预期取消路径
        if let Err(join_err) = (&mut instance.task).await {
            tracing::debug!("proxy task join after abort: {join_err}");
        }
    }
    info!("file-server 分流代理已停止, 60000 端口已释放");
    Ok(())
}

/// accept 循环 + 每连接 hyper 服务（shutdown 只停 accept，活跃连接随 task 结束断开）。
async fn serve(
    listener: tokio::net::TcpListener,
    client: ProxyClient,
    config: FileServerProxyConfig,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((io, _peer)) => {
                        let client = client.clone();
                        let config = config.clone();
                        tokio::spawn(async move {
                            let service = hyper::service::service_fn(move |req| {
                                let client = client.clone();
                                let config = config.clone();
                                async move { proxy_request(req, client, config).await }
                            });
                            let io = hyper_util::rt::TokioIo::new(io);
                            // with_upgrades: 万一上游有 WebSocket 语义不致硬拒（当前无 ws 路由）
                            let conn = hyper::server::conn::http1::Builder::new()
                                .serve_connection(io, service);
                            if let Err(e) = conn.with_upgrades().await {
                                // 客户端中断/半关闭常见, debug 级即可
                                tracing::debug!("proxy connection ended: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        // 瞬时 accept 错误（EMFILE 等）: 记日志退避继续, 不退出整个代理
                        warn!("file-server 分流代理 accept 错误: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }
}

/// 单请求转发：按 `/api/userapp*` 前缀或 `x-service-type` header 选上游，
/// 方法/路径/headers/body 原样透传。
///
/// 上游不可达返回 502（不向客户端裸抛连接错误）。
async fn proxy_request(
    req: hyper::Request<Incoming>,
    client: ProxyClient,
    config: FileServerProxyConfig,
) -> Result<hyper::Response<ProxyBody>, std::convert::Infallible> {
    let (parts, body) = req.into_parts();

    let service_type = parts
        .headers
        .get(SERVICE_TYPE_HEADER)
        .and_then(|v| v.to_str().ok());
    let port = match config.upstream_port_for(parts.uri.path(), service_type) {
        Upstream::Rust(port) => port,
        Upstream::Ts(port) => port,
    };

    let path_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());

    let mut upstream = hyper::Request::builder()
        .method(parts.method.clone())
        .uri(format!("http://127.0.0.1:{port}{path_query}"));
    if let Some(headers) = upstream.headers_mut() {
        for (name, value) in parts.headers.iter() {
            // hop-by-hop headers 不跨代理转发
            if !is_hop_by_hop(name.as_str()) {
                headers.insert(name.clone(), value.clone());
            }
        }
    }

    let upstream_req = match upstream.body(body) {
        Ok(r) => r,
        // method/headers 均来自合法入站请求, 构造失败理论不可达
        Err(e) => {
            error!("file-server 分流代理构造上游请求失败: {e}");
            return Ok(bad_gateway("build upstream request failed"));
        }
    };

    match client.request(upstream_req).await {
        Ok(resp) => {
            let (mut parts, body) = resp.into_parts();
            for h in HOP_BY_HOP {
                parts.headers.remove(h);
            }
            Ok(hyper::Response::from_parts(
                parts,
                body.map_err(std::io::Error::other).boxed(),
            ))
        }
        Err(e) => {
            error!(
                "file-server 分流代理上游 127.0.0.1:{port} 请求失败 \
                 (path {path_query}, service_type={service_type:?}): {e}"
            );
            Ok(bad_gateway(&format!(
                "upstream 127.0.0.1:{port} unreachable: {e}"
            )))
        }
    }
}

/// hop-by-hop header 集合（RFC 7231 §6.1 / 2616 §13.5.1）。
const HOP_BY_HOP: [&str; 6] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
];

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|h| name.eq_ignore_ascii_case(h))
}

/// 502 错误响应（body 归一到 ProxyBody；Full 的 error 为 Infallible，不可达分支）。
fn bad_gateway(msg: &str) -> hyper::Response<ProxyBody> {
    let body = http_body_util::Full::new(Bytes::from(msg.to_string()))
        .map_err(|e| match e {})
        .boxed();
    hyper::Response::builder()
        .status(hyper::StatusCode::BAD_GATEWAY)
        .header("content-type", "text/plain; charset=utf-8")
        .body(body)
        .expect("static 502 response parts are valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> FileServerProxyConfig {
        FileServerProxyConfig::default()
    }

    /// 主 pod 形态（UserappSplit 默认策略）：header 与 path 双判据。
    #[test]
    fn userapp_split_policy_routes_by_header_and_path() {
        let c = cfg();
        assert_eq!(c.policy, RoutePolicy::UserappSplit, "默认策略为主 pod 形态");
        // header 判据（存量路径形态 + 业务声明）
        assert_eq!(
            c.upstream_port_for("/api/computer/get-file-list", Some("userapp")),
            Upstream::Rust(8086)
        );
        // path 判据（userApp 新契约前缀；Java 未接 header 期的兜底）
        assert_eq!(
            c.upstream_port_for("/api/userapp/dev/start", None),
            Upstream::Rust(8086)
        );
        assert_eq!(
            c.upstream_port_for("/api/userapp", None),
            Upstream::Rust(8086)
        );
        // 双判据都未命中 → TS
        for path in [
            "/health",
            "/api/version",
            "/api/computer/create-workspace",
            "/api/project/list",
            "/api/git/status",
            "/api/build/start",
            "/",
        ] {
            assert_eq!(
                c.upstream_port_for(path, None),
                Upstream::Ts(60001),
                "{path}"
            );
        }
        // 段边界: /api/userapplication 不是 userApp 域
        assert_eq!(
            c.upstream_port_for("/api/userapplication", None),
            Upstream::Ts(60001)
        );
        // 非本业务域声明（computer 等）与非法值一律 TS——契约违规 404 可见而非静默误路由
        assert_eq!(
            c.upstream_port_for("/api/computer/x", Some("computer")),
            Upstream::Ts(60001)
        );
        assert_eq!(
            c.upstream_port_for("/api/computer/x", Some("UserApp")),
            Upstream::Ts(60001)
        );
        assert_eq!(
            c.upstream_port_for("/api/computer/x", Some("")),
            Upstream::Ts(60001)
        );
    }

    /// 容器形态（AllRust）：一律内嵌 Rust 上游（现状行为等价）。
    #[test]
    fn all_rust_policy_routes_everything_to_rust() {
        let c = FileServerProxyConfig {
            listen_port: 60000,
            rust_upstream_port: 60002,
            ts_upstream_port: 60001,
            policy: RoutePolicy::AllRust,
        };
        for (path, header) in [
            ("/health", None),
            ("/api/version", None),
            ("/api/computer/create-workspace", None),
            ("/api/computer/create-workspace", Some("computer")),
            ("/api/userapp/dev/start", None),
            ("/", Some("")),
        ] {
            assert_eq!(
                c.upstream_port_for(path, header),
                Upstream::Rust(60002),
                "{path} {header:?} 应一律走内嵌 Rust"
            );
        }
    }

    #[test]
    fn custom_ports_respected() {
        let c = FileServerProxyConfig {
            listen_port: 61000,
            rust_upstream_port: 18086,
            ts_upstream_port: 6001,
            policy: RoutePolicy::UserappSplit,
        };
        assert_eq!(
            c.upstream_port_for("/api/userapp/dev/start", None),
            Upstream::Rust(18086)
        );
        assert_eq!(c.upstream_port_for("/health", None), Upstream::Ts(6001));
    }

    #[test]
    fn hop_by_hop_detection() {
        assert!(is_hop_by_hop("Connection"));
        assert!(is_hop_by_hop("keep-alive"));
        assert!(!is_hop_by_hop("x-service-type"));
        assert!(!is_hop_by_hop("content-type"));
    }
}
