//! tokio-console 观测装配（`console` feature 专用，本地开发）。
//!
//! 动态 agent 容器无端口发布——bind 0.0.0.0 后宿主机经容器 IP 直连
//! （OrbStack）；端口经 `CONSOLE_BIND` 可配，默认 0.0.0.0:6669。
//! 编译须配 `RUSTFLAGS="--cfg tokio_unstable"`。

use rcoder_telemetry::TelemetryConfig;
use tracing_subscriber::Layer as _;

pub fn attach(config: TelemetryConfig) -> TelemetryConfig {
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
