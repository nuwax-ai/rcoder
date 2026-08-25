//! file-server-proxy 独立二进制入口，两种构建形态共用本 env 契约：
//!
//! - **默认（无 feature，生产运行容器形态）**：纯转发代理。supervisord 拉起本进程：
//!   60000 对外（Service 固定端口，rcoder 转发层的上游），AllRust 全量转发到本容器
//!   独立进程的 Rust file-server（8086）。与 agent-runner 容器形态
//!   （`spawn_file_server_proxy` 内嵌拉起）同款配置语义。
//! - **`--features embed-file-server`（npm 独立分发形态）**：同进程内嵌 Rust
//!   file-server，绑 `127.0.0.1:{RUST_UPSTREAM_PORT}`——单二进制 = 分流代理 +
//!   rust 上游；TS nuwax-file-server 由 npm CLI（`bin/file-server-proxy.js`）作为
//!   独立进程托管于随机端口，经 `TS_UPSTREAM_PORT` 指入。
//!
//! env 契约（非法值一律 exit(1)，报错文案可直接展示）：
//! - `FILE_SERVER_PORT`：本代理监听端口（默认 60000）
//! - `RUST_UPSTREAM_PORT`：rust 上游端口（默认 8086；embed 形态 = 内嵌 server 端口）
//! - `TS_UPSTREAM_PORT`：TS 上游端口（默认 60001；npm 形态由 CLI 注入随机端口）
//! - `ROUTE_POLICY`：`userapp_split | all_rust | all_ts`（默认 `all_rust`，容器现状）
//! - `EMBED_FILE_SERVER`（仅 embed feature 构建生效）：默认启用；`0` 禁用内嵌、
//!   回纯转发形态（上游由外部进程提供）
//!
//! 失败即退出非零（bind 冲突等）——supervisord/守护 CLI 重试，故障在进程面可见。

use file_server_proxy::{FileServerProxyConfig, RoutePolicy};
use shared_types::{AGENT_FILE_SERVER_PORT, NUWAX_FILE_SERVER_INTERNAL_PORT};

fn env_port(key: &str, default: u16) -> Result<u16, String> {
    match std::env::var(key) {
        Ok(value) => value
            .trim()
            .parse()
            .map_err(|e| format!("invalid {key}: {e}")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(format!("read {key}: {e}")),
    }
}

/// `ROUTE_POLICY` env → 策略（缺省 `all_rust` = 容器 supervisord 形态现状）。
fn env_policy() -> Result<RoutePolicy, String> {
    match std::env::var("ROUTE_POLICY") {
        Ok(value) => file_server_proxy::parse_route_policy(&value),
        Err(std::env::VarError::NotPresent) => Ok(RoutePolicy::AllRust),
        Err(e) => Err(format!("read ROUTE_POLICY: {e}")),
    }
}

fn fail(message: String) -> ! {
    eprintln!("file-server-proxy: {message}");
    std::process::exit(1);
}

/// 纯代理形态（无 embed / embed 被禁用）的日志：console fmt + `RUST_LOG`。
fn init_tracing_plain() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

#[tokio::main]
async fn main() {
    // --version/-V：供 Electron prepare 脚本核验已落地二进制的真实版本
    // （防止"版本标记更新了但二进制没换"），与 file-server bin 同款。
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("file-server-proxy {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let listen_port = match env_port("FILE_SERVER_PORT", AGENT_FILE_SERVER_PORT) {
        Ok(port) => port,
        Err(e) => fail(e),
    };
    let rust_upstream_port = match env_port("RUST_UPSTREAM_PORT", 8086) {
        Ok(port) => port,
        Err(e) => fail(e),
    };
    let ts_upstream_port = match env_port("TS_UPSTREAM_PORT", NUWAX_FILE_SERVER_INTERNAL_PORT) {
        Ok(port) => port,
        Err(e) => fail(e),
    };
    let policy = match env_policy() {
        Ok(policy) => policy,
        Err(e) => fail(e),
    };

    // 内嵌 rust 上游先于代理启动（代理转发 127.0.0.1:{rust_upstream_port}，
    // 上游不在则首个请求即 502）；guard 持有到 main 结束保证文件日志刷盘。
    #[cfg(feature = "embed-file-server")]
    let _log_guard = match prepare_embed(rust_upstream_port) {
        Ok((config, guard)) => {
            if let Some(config) = config
                && let Err(e) = spawn_embedded_file_server(config).await
            {
                fail(e);
            }
            guard
        }
        Err(e) => fail(e),
    };
    #[cfg(not(feature = "embed-file-server"))]
    {
        init_tracing_plain();
        if std::env::var("EMBED_FILE_SERVER").is_ok_and(|v| v.trim() != "0") {
            tracing::warn!(
                "EMBED_FILE_SERVER 已设置但被忽略: 二进制未编译 embed-file-server feature (纯转发形态)"
            );
        }
    }

    file_server_proxy::init(FileServerProxyConfig {
        listen_port,
        rust_upstream_port,
        ts_upstream_port,
        policy,
    });
    match file_server_proxy::try_start().await {
        Ok(address) => tracing::info!(
            "file-server-proxy (standalone) 运行中: {address} → \
             rust=127.0.0.1:{rust_upstream_port}, ts=127.0.0.1:{ts_upstream_port} ({})",
            policy.as_str()
        ),
        Err(e) => fail(e),
    }
    // 前台挂起：serve task 在后台持有监听口；supervisord/守护 CLI SIGTERM 杀进程
    // 即整体退出（内嵌 file-server task 随进程消亡，与独立进程被杀同构）。
    std::future::pending::<()>().await;
}

/// embed 形态的准备段：加载内嵌 file-server 配置 + 一次组装全局 subscriber
/// （console + file-server 按日滚动文件日志 + `RUST_LOG`/默认过滤），并在配置就绪
/// 后 spawn 内嵌 server（绑 `127.0.0.1:{config.port}`）。
///
/// `EMBED_FILE_SERVER=0` 时跳过内嵌（上游由外部进程提供，纯转发形态），返回
/// `(None, None)` 并回落纯 console 日志。guard 必须持有到 main 结束（文件日志
/// 完整刷盘）。
#[cfg(feature = "embed-file-server")]
fn prepare_embed(
    rust_upstream_port: u16,
) -> Result<
    (
        Option<file_server::Config>,
        Option<file_server::logging::WorkerGuard>,
    ),
    String,
> {
    if std::env::var("EMBED_FILE_SERVER").is_ok_and(|v| v.trim() == "0") {
        init_tracing_plain();
        eprintln!(
            "file-server-proxy: EMBED_FILE_SERVER=0, 跳过内嵌 file-server (上游由外部进程提供)"
        );
        return Ok((None, None));
    }
    let mut config = file_server::Config::load()
        .map_err(|e| format!("load embedded file-server config: {e:#}"))?;
    // proxy 进程的 FILE_SERVER_PORT 语义是代理监听口（默认 60000），内嵌 server
    // 端口必须以 RUST_UPSTREAM_PORT 显式覆盖，防两个服务解析出同一端口互相抢占。
    config.listen_host = "127.0.0.1".to_string();
    config.port = rust_upstream_port;

    let (file_layer, guard) = file_server::logging::build_file_layer(&config)
        .map_err(|e| format!("build embedded file-server log layer: {e:#}"))?;
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "file_server=info,file_server_proxy=info,tower_http=info",
        )
    });
    let console = tracing_subscriber::fmt::layer().with_target(true);
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::registry()
        .with(file_layer)
        .with(filter)
        .with(console)
        .init();
    Ok((Some(config), Some(guard)))
}

/// 内嵌 Rust file-server：同进程 tokio task 绑 `127.0.0.1:{config.port}`，
/// 转发/排障路径与外部独立 file-server 进程完全同构（仅少一个进程）。
/// 无程序化停机——进程退出即消亡（守护方 SIGTERM 杀进程组），shutdown 传
/// pending 让 serve 只随进程终止。
#[cfg(feature = "embed-file-server")]
async fn spawn_embedded_file_server(config: file_server::Config) -> Result<(), String> {
    let address = format!("{}:{}", config.listen_host, config.port);
    let server = file_server::FileServer::builder(config)
        .build()
        .map_err(|e| format!("build embedded file-server: {e:#}"))?;
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .map_err(|e| format!("bind embedded file-server {address} 失败: {e}"))?;
    tracing::info!("内嵌 file-server 运行中: {address}");
    tokio::spawn(async move {
        if let Err(e) = server
            .serve_with_shutdown(listener, std::future::pending())
            .await
        {
            tracing::error!("内嵌 file-server serve 退出: {e}");
        }
    });
    Ok(())
}
