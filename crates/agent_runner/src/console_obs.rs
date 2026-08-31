//! tokio-console 观测装配（`console` feature 专用，本地开发）。
//!
//! 动态 agent 容器无端口发布——bind 0.0.0.0 后宿主机经容器 IP 直连
//! （OrbStack）；端口经 `CONSOLE_BIND` 可配，默认 0.0.0.0:6669。
//! 编译须配 `RUSTFLAGS="--cfg tokio_unstable"`。
//!
//! 运行期开关 `DEV_CONSOLE`（默认关，与 rcoder 主进程同语义）：仅
//! `DEV_CONSOLE=1` 时注入 ConsoleLayer；关闭时 EnvFilter 不放行
//! tokio/runtime trace 事件，开销近零。不能常驻的原因见 rcoder 侧
//! console_obs 文档（console-subscriber 无背压记账，RSS 持续爬升）。

use rcoder_telemetry::TelemetryConfig;
use tracing_subscriber::Layer as _;

pub fn attach(config: TelemetryConfig) -> TelemetryConfig {
    if !std::env::var("DEV_CONSOLE").is_ok_and(|v| v == "1") {
        eprintln!("[AGENT_RUNNER] tokio-console disabled (set DEV_CONSOLE=1 to enable)");
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
