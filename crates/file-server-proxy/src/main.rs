//! file-server-proxy 独立二进制入口（生产运行容器形态）。
//!
//! 生产 UserApp 运行容器经 supervisord 拉起本进程：60000 对外（Service 固定端口，
//! rcoder 转发层的上游），AllRust 全量转发到本容器的 Rust file-server（8086）。
//! 与 agent-runner 容器形态（`spawn_file_server_proxy` 内嵌拉起）同款配置语义，
//! env 契约一致：`FILE_SERVER_PORT` = 本代理监听端口，`RUST_UPSTREAM_PORT` =
//! file-server 端口。
//!
//! 失败即退出非零（bind 冲突等）——supervisord 重试，故障在进程面可见。

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

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let listen_port = match env_port("FILE_SERVER_PORT", AGENT_FILE_SERVER_PORT) {
        Ok(port) => port,
        Err(e) => {
            eprintln!("file-server-proxy: {e}");
            std::process::exit(1);
        }
    };
    let rust_upstream_port = match env_port("RUST_UPSTREAM_PORT", 8086) {
        Ok(port) => port,
        Err(e) => {
            eprintln!("file-server-proxy: {e}");
            std::process::exit(1);
        }
    };

    file_server_proxy::init(FileServerProxyConfig {
        listen_port,
        rust_upstream_port,
        ts_upstream_port: NUWAX_FILE_SERVER_INTERNAL_PORT,
        policy: RoutePolicy::AllRust,
    });
    match file_server_proxy::try_start().await {
        Ok(address) => {
            tracing::info!(
                "file-server-proxy (standalone) 运行中: {address} → 127.0.0.1:{rust_upstream_port} (all_rust)"
            );
        }
        Err(e) => {
            eprintln!("file-server-proxy: {e}");
            std::process::exit(1);
        }
    }
    // 前台挂起：serve task 在后台持有 60000；supervisord SIGTERM 杀进程组即整体退出。
    std::future::pending::<()>().await;
}
