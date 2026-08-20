//! tokio-console 观测装配（`console` feature 专用，本地开发）。
//!
//! bind `0.0.0.0`（console-subscriber 默认 127.0.0.1，容器内 bind 127
//! 宿主机/跨容器不可达）；端口经 `CONSOLE_BIND` 可配，默认 0.0.0.0:6669。
//! 编译须配 `RUSTFLAGS="--cfg tokio_unstable"`（make run-console /
//! DEV_CONSOLE=1 make dev-hot 已封装）。

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
            eprintln!("[BOOTSTRAP] tokio-console enabled: {bind}");
            config.with_console_layer(layer)
        }
        Err(e) => {
            eprintln!("[BOOTSTRAP] CONSOLE_BIND invalid ({bind}: {e}), tokio-console disabled");
            config
        }
    }
}
