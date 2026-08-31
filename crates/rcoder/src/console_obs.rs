//! tokio-console 观测装配（`console` feature 专用，本地开发）。
//!
//! bind `0.0.0.0`（console-subscriber 默认 127.0.0.1，容器内 bind 127
//! 宿主机/跨容器不可达）；端口经 `CONSOLE_BIND` 可配，默认 0.0.0.0:6669。
//! 编译须配 `RUSTFLAGS="--cfg tokio_unstable"`（make dev-hot 恒定携带）。
//!
//! 运行期开关 `DEV_CONSOLE`（默认关）：console feature 恒编入 binary，
//! 仅 `DEV_CONSOLE=1` 时注入 ConsoleLayer。关闭时不 attach + EnvFilter 不
//! 放行 tokio/runtime trace 事件——tokio instrumentation 事件在 EnvFilter
//! 处即被丢弃（Registry 不存储、Aggregator 不启动），开销近零。不能常驻的
//! 原因：console-subscriber 对 tokio 微观事件（每次 channel 收发/wake）
//! 无背压记账，rcoder 后台负载（status checker 轮询 + OTLP 导出）下
//! RSS 以数十 MB/s 爬升直至 OOM；运行期开关让"用时开、用完关"免去
//! 功能切换的重编译等待（`make console-on` / `make console-off`）。

use rcoder_telemetry::TelemetryConfig;
use tracing_subscriber::Layer as _;

pub fn attach(config: TelemetryConfig) -> TelemetryConfig {
    // 运行期开关默认关：DEV_CONSOLE 未设或非 1 均不注入。与 compose 的
    // `DEV_CONSOLE=${DEV_CONSOLE:-0}` 联动（make console-on/off 重建容器切换）。
    if std::env::var("DEV_CONSOLE").is_ok_and(|v| v == "1") {
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
    } else {
        eprintln!("[BOOTSTRAP] tokio-console disabled (set DEV_CONSOLE=1 to enable)");
        config
    }
}
