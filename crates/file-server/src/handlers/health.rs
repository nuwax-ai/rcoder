//! 健康检查 HTTP handlers。
//!
//! 契约: 返回 HTTP 200 + `{ "status": "ok", ... }`。
//! `start-services.sh` 周期 curl 此端点, 连续失败会 kill + 重启 file-server,
//! 因此 `status: "ok"` 字段不可缺 (对齐 nuwax `routes/router.js` 的 /health)。
//!
//! 响应字段对齐 nuwax: timestamp/uptime/version/platform/runtimeVersion/pid/memory/env。

use axum::Json;
use axum::extract::State;
use axum::response::Html;

use crate::AppState;
use crate::models::{HealthResponse, MemoryUsage, VersionResponse};

/// 根路径探测（兼容 nuwax）
#[utoipa::path(
    get,
    path = "/",
    responses((status = 200, description = "Service greeting", body = String, content_type = "text/html")),
    tag = "System"
)]
pub async fn root() -> Html<&'static str> {
    Html("Hello")
}

/// file-server 产品契约版本。
///
/// 消费方用它做能力门禁: Java `FileServerVersionSupport` 读取 `/api/version` 并与
/// `AGENT_STORE_MIN_VERSION = 1.4.0` 比较, 决定是否走 v2 agent-store API。本实现
/// 已覆盖该契约面 (create-workspace-v2/agent-store), 因此报告所实现的 TS 契约线
/// 版本; crate 版本只是构建编号, 不参与能力判断。升级功能面时同步提升此常量。
pub const API_CONTRACT_VERSION: &str = "1.5.3";

/// 健康检查
#[utoipa::path(
    get,
    path = "/health",
    responses((status = 200, description = "Service health", body = HealthResponse)),
    tag = "System"
)]
pub async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let uptime = state.started_at.elapsed().as_secs();
    let memory = memory_usage();
    Json(HealthResponse {
        status: "ok".to_string(),
        timestamp: now_ms(),
        uptime,
        version: API_CONTRACT_VERSION.to_string(),
        platform: std::env::consts::OS.to_string(),
        // Rust 无 node 运行时, 用 rust edition 标识 (对齐 nuwax nodeVersion 字段位)
        node_version: "rust-2024".to_string(),
        pid: std::process::id(),
        memory,
        env: std::env::var("NODE_ENV").unwrap_or_else(|_| "unknown".to_string()),
    })
}

/// 版本协商
///
/// 版本协商 (对齐 TS v1.4.0 router.js)。
/// Java 网关据此决定走 v2 新 API (agent-store) 还是旧 API, 因此必须报告
/// [`API_CONTRACT_VERSION`] (契约能力线), 不能报告 crate 构建号——后者会让
/// Java 的 `>= 1.4.0` 门禁误判为不支持 agent-store。
#[utoipa::path(
    get,
    path = "/api/version",
    responses((status = 200, description = "Service version", body = VersionResponse)),
    tag = "System"
)]
pub async fn version() -> Json<VersionResponse> {
    Json(VersionResponse {
        success: true,
        version: API_CONTRACT_VERSION.to_string(),
    })
}

/// 当前 epoch 毫秒 (对齐 nuwax Date.now())。
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 内存占用 (MB, 对齐 nuwax process.memoryUsage 形状; Rust 无 GC 堆, heapUsed/Total/external 填 0,
/// rss 取 /proc/self/status 的 VmRSS, 不可用时为 0)。
fn memory_usage() -> MemoryUsage {
    let round2 = |mb: f64| (mb * 100.0).round() / 100.0;
    MemoryUsage {
        rss: round2(rss_mb()),
        heap_used: 0.0,
        heap_total: 0.0,
        external: 0.0,
    }
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
