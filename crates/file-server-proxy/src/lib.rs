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

/// 60000（对外入口）与 60001（TS 内部端口）的单一事实源见 shared_types。
pub use shared_types::{AGENT_FILE_SERVER_PORT, NUWAX_FILE_SERVER_INTERNAL_PORT};

/// 统一响应 body（上游 Incoming / 直连 axum Body 与本地错误 Full 的归一）。
/// hyper 连接层只要求 `HttpBody + Send`——不设 Sync 约束（axum 的 `Body`
/// 非 Sync，`BoxBody` 的 Sync 要求会把直连路径挡在门外）。
pub type ProxyBody =
    std::pin::Pin<Box<dyn http_body::Body<Data = Bytes, Error = std::io::Error> + Send>>;

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
            listen_port: AGENT_FILE_SERVER_PORT,
            rust_upstream_port: 8086,
            ts_upstream_port: NUWAX_FILE_SERVER_INTERNAL_PORT,
            policy: RoutePolicy::default(),
        }
    }
}

/// 路由策略——同一 crate 服务多种部署形态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutePolicy {
    /// 分流模式（现状兼容，生产在跑）：userApp 判据（path 前缀或
    /// `x-service-type` header）→ rust 上游，其余 → ts 上游
    /// （存量域继续 TS nuwax-file-server；TS 尚未支持 service_type 入参时
    /// 存量路径上的 userApp 业务必须由 Rust 承载）
    #[default]
    UserappSplit,
    /// 全 Rust 模式（切流终态）：一律 rust 上游，全部流量由 Rust 重写的
    /// file-server 承载（TS 热备于 [`NUWAX_FILE_SERVER_INTERNAL_PORT`]，不接流量）
    AllRust,
    /// 全 TS 模式（npm 独立形态的回退/AB 对照档）：一律 ts 上游。
    /// 无路径白名单（TS 本就是全量老路由面，白名单语义不适用）；
    /// TS 没有的 userApp 新接口（/api/userapp/*）在此模式下由 TS 返回 404。
    AllTs,
    /// TS 优先模式（过渡切流档）：**仅** Rust 独有接口（`/api/userapp*`，TS 无
    /// 此路由）→ rust 上游；存量同名接口**全走 TS**（含带 `x-service-type`
    /// 标记的请求——header 判据在此模式下失效，由 TS 以 service_type 入参
    /// 内部消费 userApp 业务）。验证 TS 侧 userApp 能力就绪后的整体切流形态。
    TsFirst,
}

impl RoutePolicy {
    /// 策略的 wire 值（serde/CLI/env/helm 共用词汇表）。
    pub const fn as_str(self) -> &'static str {
        match self {
            RoutePolicy::UserappSplit => "userapp_split",
            RoutePolicy::AllRust => "all_rust",
            RoutePolicy::AllTs => "all_ts",
            RoutePolicy::TsFirst => "ts_first",
        }
    }
}

/// 解析策略值（env/CLI 入口共用；serde 之外的运行时入口）。
///
/// 受认可值与 serde wire 契约一致：`userapp_split|all_rust|all_ts|ts_first`。
/// 非法值返回 Err（带受认可值清单，调用方 exit 前可直接展示）。
pub fn parse_route_policy(value: &str) -> Result<RoutePolicy, String> {
    match value.trim() {
        "userapp_split" => Ok(RoutePolicy::UserappSplit),
        "all_rust" => Ok(RoutePolicy::AllRust),
        "all_ts" => Ok(RoutePolicy::AllTs),
        "ts_first" => Ok(RoutePolicy::TsFirst),
        other => Err(format!(
            "invalid route policy {other:?}: expected one of \
             userapp_split | all_rust | all_ts | ts_first"
        )),
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
    /// - [`RoutePolicy::AllTs`]：一律 TS 上游
    /// - [`RoutePolicy::TsFirst`]：仅 `/api/userapp*` → Rust 上游（header 判据
    ///   失效，存量同名接口含 userApp 标记一律 TS）
    pub fn upstream_port_for(&self, path: &str, service_type_header: Option<&str>) -> Upstream {
        let to_rust = match self.policy {
            RoutePolicy::UserappSplit => {
                is_userapp_path(path) || is_userapp_service_type(service_type_header)
            }
            RoutePolicy::AllRust => true,
            RoutePolicy::AllTs => false,
            RoutePolicy::TsFirst => is_userapp_path(path),
        };
        if to_rust {
            Upstream::Rust(self.rust_upstream_port)
        } else {
            Upstream::Ts(self.ts_upstream_port)
        }
    }
}

// ── 全局生命周期管理（admin API / CLI 复刻自 f55f230 的内嵌 file-server 模式）──

/// 进程内直连通道（feature `embed-file-server`；npm/Electron 独立形态）。
///
/// 设置后 rust 域请求不再经 loopback 转发 `127.0.0.1:{rust_upstream_port}`，
/// 而是直接 `router.oneshot(request)` 进程内调用（file-server 以 lib 集成，
/// 单二进制单监听口）。容器/rcoder 嵌入形态不设置本值，转发路径原样。
#[cfg(feature = "embed-file-server")]
mod in_process {
    use std::sync::RwLock;

    static ROUTER: RwLock<Option<axum::Router>> = RwLock::new(None);

    /// 注册直连 router（幂等覆盖；bin 启动装配时调用）。
    pub fn set_in_process_router(router: axum::Router) {
        match ROUTER.write() {
            Ok(mut guard) => *guard = Some(router),
            Err(poisoned) => {
                *poisoned.into_inner() = Some(router);
            }
        }
    }

    /// 清除直连 router（测试复位；清除后回 loopback 转发路径）。
    pub fn clear_in_process_router() {
        match ROUTER.write() {
            Ok(mut guard) => *guard = None,
            Err(poisoned) => {
                *poisoned.into_inner() = None;
            }
        }
    }

    /// 取当前直连 router 的克隆（per-request clone 是 axum 官方模式，Arc 浅拷贝）。
    pub(super) fn take() -> Option<axum::Router> {
        ROUTER.read().ok().and_then(|guard| guard.clone())
    }
}

#[cfg(feature = "embed-file-server")]
pub use in_process::{clear_in_process_router, set_in_process_router};

/// 直连调用（feature 门控）：oneshot 进 file-server 的 axum Router，响应 body
/// 归一到 [`ProxyBody`]。handler 层错误（路由/中间件 panic 被 tower 捕获等）→ 502。
#[cfg(feature = "embed-file-server")]
async fn call_in_process(
    router: axum::Router,
    req: hyper::Request<Incoming>,
) -> hyper::Response<ProxyBody> {
    use tower::ServiceExt;
    match router.oneshot(req).await {
        // 直连不经网络代理跳，hop-by-hop 头不做剥除（我们即服务器，与独立
        // file-server bin 直连行为一致；hyper 连接层的 keep-alive 语义照常）
        Ok(resp) => resp.map(|body| Box::pin(body.map_err(std::io::Error::other)) as ProxyBody),
        Err(e) => {
            error!("file-server 分流代理直调内嵌 file-server 失败: {e}");
            bad_gateway("file-server upstream error")
        }
    }
}

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
///
/// 顺带自愈：serve task 已死（意外退出且 spawn 内 cleanup 被 panic 跳过等）时
/// 就地清掉死实例，避免 status 误报 running。
pub async fn status() -> Option<String> {
    let mut guard = INSTANCE.lock().await;
    reap_dead_instance(&mut guard);
    guard.as_ref().map(|i| i.address.clone())
}

/// map 内实例的 task 已结束则清掉（幂等；serve panic 跳过 spawn 内 cleanup 的兜底）。
fn reap_dead_instance(guard: &mut tokio::sync::MutexGuard<'_, Option<RunningInstance>>) {
    if guard.as_ref().is_some_and(|i| i.task.is_finished()) {
        guard.take();
        warn!("file-server 分流代理 serve 已退出, 死实例状态自愈为已停止");
    }
}

/// 启动分流代理（幂等）。同步 bind（而非 spawn 内 bind），返回时状态准确。
pub async fn try_start() -> Result<String, String> {
    let mut guard = INSTANCE.lock().await;
    reap_dead_instance(&mut guard);
    if let Some(instance) = guard.as_ref() {
        return Ok(instance.address.clone());
    }
    let config = CONFIG.get().cloned().unwrap_or_else(|| {
        warn!("file-server-proxy 配置未 init, 回落默认端口 (60000 → 8086/60001)");
        FileServerProxyConfig::default()
    });

    let address = format!("0.0.0.0:{}", config.listen_port);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|e| format!("bind {address} 失败（端口被占用?）: {e}"))?;

    // 上游 hang 防堆积: 连接 5s 建立超时; 整请求超时在 proxy_request 内包装
    let mut connector = hyper_util::client::legacy::connect::HttpConnector::new();
    connector.set_connect_timeout(Some(std::time::Duration::from_secs(5)));
    let client: ProxyClient =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build(connector);

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
                            // 注: 本代理不透传 WebSocket(upgrade 已列入 hop-by-hop 剥除);
                            // with_upgrades 仅为 hyper 连接层的 upgrade 协商宽容,
                            // 避免 h1 客户端带 upgrade 头时连接被硬断
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

    let path = parts.uri.path();
    let service_type = parts
        .headers
        .get(SERVICE_TYPE_HEADER)
        .and_then(|v| v.to_str().ok());
    let port = match config.upstream_port_for(path, service_type) {
        Upstream::Rust(port) => {
            // AllRust 白名单: 60000 只放行 file-server 语义路径, 不把上游 8086 的
            // 全量路由面（/chat、/agent-mgmt/* 等集群内面）推到对外入口
            if config.policy == RoutePolicy::AllRust && !all_rust_path_allowed(path) {
                warn!("AllRust 白名单外路径已拒绝: {path}");
                return Ok(not_found("path not served on this entry"));
            }
            // 进程内直连（embed 形态装配后）：原请求重组后直接 oneshot 进
            // file-server 的 axum Router，不经 loopback 转发
            #[cfg(feature = "embed-file-server")]
            if let Some(router) = in_process::take() {
                return Ok(call_in_process(router, hyper::Request::from_parts(parts, body)).await);
            }
            port
        }
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
    let Some(headers) = upstream.headers_mut() else {
        // 仅 asterisk-form (`OPTIONS *`) 等异常 request-target 会走到这里
        error!("file-server 分流代理构造上游请求失败: 无效 request-target {path_query:?}");
        return Ok(bad_request("invalid request-target"));
    };
    for (name, value) in parts.headers.iter() {
        // hop-by-hop headers 不跨代理转发; append 保多值 header (Cookie 链等)
        if !is_hop_by_hop(name.as_str()) {
            headers.append(name.clone(), value.clone());
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

    // 整请求 300s 超时（宽限大文件上传/慢接口; 防"上游接受连接后不响应"无限堆积）
    const UPSTREAM_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
    let upstream_result =
        match tokio::time::timeout(UPSTREAM_REQUEST_TIMEOUT, client.request(upstream_req)).await {
            Ok(result) => result,
            Err(_elapsed) => {
                error!(
                    "file-server 分流代理上游 127.0.0.1:{port} 请求超时 \
                     ({UPSTREAM_REQUEST_TIMEOUT:?}, path {path_query})"
                );
                return Ok(bad_gateway("file-server upstream timeout"));
            }
        };
    match upstream_result {
        Ok(resp) => {
            let (mut parts, body) = resp.into_parts();
            for h in HOP_BY_HOP {
                parts.headers.remove(h);
            }
            Ok(hyper::Response::from_parts(
                parts,
                Box::pin(body.map_err(std::io::Error::other)),
            ))
        }
        Err(e) => {
            // 对外文案不泄露内部拓扑, 详情在日志
            error!(
                "file-server 分流代理上游 127.0.0.1:{port} 请求失败 \
                 (path {path_query}, service_type={service_type:?}): {e}"
            );
            Ok(bad_gateway("file-server upstream unavailable"))
        }
    }
}

/// hop-by-hop header 集合（RFC 7231 §6.1 / 2616 §13.5.1）。
const HOP_BY_HOP: [&str; 7] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    // upgrade 也是 hop-by-hop：本代理不透传 WebSocket（当前 file-server 无 ws 路由），
    // 剥除可防"孤立 upgrade 头"到达上游引发歧义
    "upgrade",
];

/// AllRust 模式的 60000 入口白名单：file-server 语义路径（`/api/*`、`/health`、`/`、
/// swagger `/api-docs*`）。UserappSplit 的 rust 分支无需白名单——其判据本身已窄面。
fn all_rust_path_allowed(path: &str) -> bool {
    path == "/health" || path == "/" || path.starts_with("/api/") || path.starts_with("/api-docs")
}

fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|h| name.eq_ignore_ascii_case(h))
}

/// 502 错误响应（body 归一到 ProxyBody；Full 的 error 为 Infallible，不可达分支）。
fn bad_gateway(msg: &str) -> hyper::Response<ProxyBody> {
    error_response(hyper::StatusCode::BAD_GATEWAY, msg)
}

/// 400 错误响应（异常 request-target 等）。
fn bad_request(msg: &str) -> hyper::Response<ProxyBody> {
    error_response(hyper::StatusCode::BAD_REQUEST, msg)
}

/// 404：AllRust 白名单外的路径（此入口不服务该路径——不放行 8086 全量路由面）。
fn not_found(msg: &str) -> hyper::Response<ProxyBody> {
    error_response(hyper::StatusCode::NOT_FOUND, msg)
}

fn error_response(status: hyper::StatusCode, msg: &str) -> hyper::Response<ProxyBody> {
    let body: ProxyBody =
        Box::pin(http_body_util::Full::new(Bytes::from(msg.to_string())).map_err(|e| match e {}));
    hyper::Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(body)
        .expect("static error response parts are valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> FileServerProxyConfig {
        FileServerProxyConfig::default()
    }

    /// config.yml wire 契约：策略值为 snake_case（helm 模板渲染依赖此形态）。
    #[test]
    fn policy_serializes_snake_case() {
        assert_eq!(
            serde_yaml::to_string(&RoutePolicy::UserappSplit)
                .unwrap()
                .trim(),
            "userapp_split"
        );
        assert_eq!(
            serde_yaml::to_string(&RoutePolicy::AllRust).unwrap().trim(),
            "all_rust"
        );
        assert_eq!(
            serde_yaml::to_string(&RoutePolicy::AllTs).unwrap().trim(),
            "all_ts"
        );
        assert_eq!(
            serde_yaml::to_string(&RoutePolicy::TsFirst).unwrap().trim(),
            "ts_first"
        );
        let parsed: RoutePolicy = serde_yaml::from_str("all_rust").unwrap();
        assert_eq!(parsed, RoutePolicy::AllRust);
        let parsed: RoutePolicy = serde_yaml::from_str("all_ts").unwrap();
        assert_eq!(parsed, RoutePolicy::AllTs);
        let parsed: RoutePolicy = serde_yaml::from_str("ts_first").unwrap();
        assert_eq!(parsed, RoutePolicy::TsFirst);
        // 段缺 policy 字段 → 默认 UserappSplit（存量 config 兼容）
        let parsed: FileServerProxyConfig = serde_yaml::from_str(
            "listen_port: 60000\nrust_upstream_port: 8086\nts_upstream_port: 60001\n",
        )
        .unwrap();
        assert_eq!(parsed.policy, RoutePolicy::UserappSplit);
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

    /// 全 TS 模式（AllTs）：一律 TS 上游——userApp 判据在此模式下不生效
    /// （后端选择整体切 TS 时不存在"部分路径仍走 Rust"的语义）。
    #[test]
    fn all_ts_policy_routes_everything_to_ts() {
        let c = FileServerProxyConfig {
            listen_port: 60000,
            rust_upstream_port: 8086,
            ts_upstream_port: 41234,
            policy: RoutePolicy::AllTs,
        };
        for (path, header) in [
            ("/health", None),
            ("/api/userapp/dev/start", None),
            // userApp 显式 header 也走 TS——全 TS 语义优先于域判据
            ("/api/computer/get-file-list", Some("userapp")),
            ("/api/version", None),
            ("/", None),
        ] {
            assert_eq!(
                c.upstream_port_for(path, header),
                Upstream::Ts(41234),
                "{path} {header:?} 应一律走 TS"
            );
        }
    }

    /// parse_route_policy：与 serde wire 词汇表一致（含容差 trim 与非法值报错）。
    #[test]
    fn parse_route_policy_accepts_wire_vocabulary() {
        assert_eq!(
            parse_route_policy("userapp_split").unwrap(),
            RoutePolicy::UserappSplit
        );
        assert_eq!(
            parse_route_policy("all_rust").unwrap(),
            RoutePolicy::AllRust
        );
        assert_eq!(parse_route_policy("all_ts").unwrap(), RoutePolicy::AllTs);
        assert_eq!(
            parse_route_policy("ts_first").unwrap(),
            RoutePolicy::TsFirst
        );
        // trim 容差（env 值尾随空白是常见脏数据）
        assert_eq!(parse_route_policy(" all_ts\n").unwrap(), RoutePolicy::AllTs);
        // as_str 与 parse 互逆
        for policy in [
            RoutePolicy::UserappSplit,
            RoutePolicy::AllRust,
            RoutePolicy::AllTs,
            RoutePolicy::TsFirst,
        ] {
            assert_eq!(parse_route_policy(policy.as_str()).unwrap(), policy);
        }
        // 非法值：报错文案带受认可值清单（调用方直接展示给用户）
        for bad in ["", "split", "ALL_RUST", "userapp", "ts-first"] {
            let err = parse_route_policy(bad).unwrap_err();
            assert!(
                err.contains("userapp_split | all_rust | all_ts | ts_first"),
                "{bad:?} 报错应含受认可值清单: {err}"
            );
        }
    }

    /// TS 优先模式（TsFirst）：存量同名接口全走 TS——**含 userApp 标记**
    /// （header 判据失效，由 TS 以 service_type 入参消费）；仅 Rust 独有的
    /// `/api/userapp*` 走 rust。与 UserappSplit 的差异点就在 header 判据。
    #[test]
    fn ts_first_policy_routes_legacy_to_ts_even_with_userapp_header() {
        let c = FileServerProxyConfig {
            listen_port: 60000,
            rust_upstream_port: 8086,
            ts_upstream_port: 60001,
            policy: RoutePolicy::TsFirst,
        };
        // Rust 独有接口 → rust
        for path in [
            "/api/userapp",
            "/api/userapp/dev/start",
            "/api/userapp/files",
        ] {
            assert_eq!(
                c.upstream_port_for(path, None),
                Upstream::Rust(8086),
                "{path}"
            );
        }
        // 存量同名接口 → TS；带 userApp 标记也走 TS（差异点/语义铁证）
        for (path, header) in [
            ("/api/computer/get-file-list", None),
            ("/api/computer/get-file-list", Some("userapp")),
            ("/api/project/content", Some("userapp")),
            ("/health", None),
            ("/api/version", None),
            ("/api/userapplication", None),
        ] {
            assert_eq!(
                c.upstream_port_for(path, header),
                Upstream::Ts(60001),
                "{path} {header:?} 应走 TS"
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
        assert!(is_hop_by_hop("upgrade"));
        assert!(!is_hop_by_hop("x-service-type"));
        assert!(!is_hop_by_hop("content-type"));
    }

    /// AllRust 白名单: file-server 语义路径放行, 上游 8086 的其余路由面
    /// （/chat、/agent-mgmt/* 等集群内面）不放行——防 60000 入口裸暴露。
    #[test]
    fn all_rust_whitelist_gates_rust_upstream_surface() {
        for path in [
            "/api/version",
            "/api/computer/create-workspace",
            "/api/userapp/dev/start",
            "/health",
            "/",
            "/api-docs/openapi.json",
        ] {
            assert!(all_rust_path_allowed(path), "{path} 应放行");
        }
        for path in [
            "/chat",
            "/computer/chat",
            "/agent/stop",
            "/agent-mgmt/agents/install-from-url",
            "/ready",
            "/proxy/3000/x",
        ] {
            assert!(!all_rust_path_allowed(path), "{path} 应拒绝(白名单外)");
        }
    }
}
