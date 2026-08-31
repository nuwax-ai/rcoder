//! tokio-console 观测装配（`console` feature 专用，本地开发）。
//!
//! 动态 agent 容器无端口发布——bind 0.0.0.0 后宿主机经容器 IP 直连
//! （OrbStack）；端口经 `CONSOLE_BIND` 可配，默认 0.0.0.0:6669。
//! 编译须配 `RUSTFLAGS="--cfg tokio_unstable"`。
//!
//! 运行期开关 `DEV_CONSOLE`（默认开——与 rcoder 主进程相反）：动态 agent
//! 容器生命周期短且 feature 经 AGENT_CONSOLE 构建参数显式选择（构建即想
//! 观测），无"常驻忘了关"的 OOM 场景；`DEV_CONSOLE=0` 为逃生口。rcoder
//! 主进程默认关的原因见其 console_obs 文档（常驻进程 + 后台事件量下
//! RSS 持续爬升）。

use rcoder_telemetry::TelemetryConfig;
use tracing_subscriber::Layer as _;

pub fn attach(config: TelemetryConfig) -> TelemetryConfig {
    if std::env::var("DEV_CONSOLE").is_ok_and(|v| v == "0") {
        eprintln!("[AGENT_RUNNER] tokio-console disabled (DEV_CONSOLE=0)");
        return config;
    }
    let bind = std::env::var("CONSOLE_BIND").unwrap_or_else(|_| "0.0.0.0:6669".to_owned());
    match bind.parse::<std::net::SocketAddr>() {
        Ok(addr) => {
            let layer = console_subscriber::ConsoleLayer::builder()
                .server_addr(addr)
                .spawn()
                .boxed();
            eprintln!("[AGENT_RUNNER] tokio-console enabled: {bind}");
            config.with_console_layer(layer)
        }
        Err(e) => {
            eprintln!("[AGENT_RUNNER] CONSOLE_BIND invalid ({bind}: {e}), tokio-console disabled");
            config
        }
    }
}
