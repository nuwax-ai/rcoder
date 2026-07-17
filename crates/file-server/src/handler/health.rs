//! 健康检查端点。
//!
//! 契约: 返回 HTTP 200 + `{ "status": "ok", ... }`。
//! `start-services.sh` 周期 curl 此端点, 连续失败会 kill + 重启 file-server,
//! 因此 `status: "ok"` 字段不可缺 (对齐 nuwax `routes/router.js` 的 /health)。
//!
//! 响应字段对齐 nuwax: timestamp/uptime/version/platform/runtimeVersion/pid/memory/env。

use axum::Json;
use serde_json::{Value, json};

/// 进程启动时间 (首次调用 health 时记; 用于 uptime 计算)。
static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// `GET /health`
pub async fn health() -> Json<Value> {
    let start = START.get_or_init(std::time::Instant::now);
    let uptime = start.elapsed().as_secs();
    let memory = memory_usage();
    Json(json!({
        "status": "ok",
        "service": "file-server",
        "timestamp": now_ms(),
        "uptime": uptime,
        "version": env!("CARGO_PKG_VERSION"),
        "platform": std::env::consts::OS,
        // Rust 无 node 运行时, 用 rust edition 标识 (对齐 nuwax nodeVersion 字段位)
        "nodeVersion": "rust-2024",
        "pid": std::process::id(),
        "memory": memory,
        "env": std::env::var("NODE_ENV").unwrap_or_else(|_| "unknown".to_string()),
    }))
}

/// 当前 epoch 毫秒 (对齐 nuwax Date.now())。
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 内存占用 (MB, 对齐 nuwax process.memoryUsage 形状; Rust 无 GC 堆, heapUsed/Total/external 填 0,
/// rss 取 /proc/self/status 的 VmRSS, 不可用时为 0)。
fn memory_usage() -> Value {
    let round2 = |mb: f64| (mb * 100.0).round() / 100.0;
    json!({
        "rss": round2(rss_mb()),
        "heapUsed": 0.0,
        "heapTotal": 0.0,
        "external": 0.0,
    })
}

/// 读 /proc/self/status 的 VmRSS (KB → MB); 非 Linux 或读取失败 → 0。
fn rss_mb() -> f64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    let kb: f64 = rest
                        .trim()
                        .split_whitespace()
                        .next()
                        .and_then(|t| t.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    return kb / 1024.0;
                }
            }
        }
        0.0
    }
    #[cfg(not(target_os = "linux"))]
    {
        0.0
    }
}
