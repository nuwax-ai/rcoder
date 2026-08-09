//! 健康检查处理器
//!
//! 由 binary 端的 router 注册，lib 内不直接调用。
//!
//! 健康检查逻辑：
//! - HTTP 服务：本端点正常响应即表示就绪
//! - gRPC 服务（可选）：当启用 grpc-server feature 时，会检查本地 gRPC 端口
//!
//! 只有当所有启用的服务都就绪时，才返回 "healthy" 状态。

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::http::StatusCode;
use shared_types::HttpResult;
use tracing::{debug, info};

/// 上次 readiness 探测报告的 gRPC 就绪状态 (用于翻转检测)。
///
/// K8s readinessProbe 每秒打一次 `/ready`, 若每次都 info 会刷屏 stdout/容器日志。
/// 改为: 状态翻转(not_ready↔ready)时打 info(运维关心"何时就绪"), 稳态打 debug。
static LAST_REPORTED_GRPC_READY: AtomicBool = AtomicBool::new(false);

/// 健康检查端点
///
/// 检查服务是否完全就绪：
/// - HTTP 服务：本端点正常响应即表示就绪
/// - gRPC 服务（可选）：当启用 grpc-server feature 时，检查本地 50051 端口
///
/// 只有当所有启用的服务都就绪时，才返回 "healthy" 状态。
/// 这样 RCoder 只需检查 HTTP /health 端点即可确保服务完全可用。
///
/// 重要：当 gRPC 未就绪时，返回 HTTP 503 状态码，这样 K8s readinessProbe
/// 会认为 Pod 不是 Ready 的，避免 rcoder 过早连接 gRPC。
#[utoipa::path(
    get,
    path = "/ready",
    responses(
        (status = 200, description = "服务就绪", body = HttpResult<shared_types::HealthCheckResponse>),
        (status = 503, description = "服务未就绪（gRPC 未启动）", body = HttpResult<shared_types::HealthCheckResponse>)
    ),
    tag = "system"
)]
/// readiness 检查端点（/ready）：gRPC(50051) 就绪才返回 200，否则 503。
/// 供 K8s readinessProbe —— gRPC 没起时摘流量，起了再放行。
/// 与 /health（liveness，纯进程活恒 200）分离：避免 gRPC 启动期 readiness 过早放流量。
pub async fn ready_check() -> (
    StatusCode,
    Json<HttpResult<shared_types::HealthCheckResponse>>,
) {
    // HTTP 服务：本端点正常响应即表示 HTTP 就绪
    let http_ready = true;

    // 检查 gRPC 端口是否就绪。check_grpc_port_simple 无 feature gate,
    // 任何构建都真正探测 50051；不绑 grpc-server feature, 避免"未启 feature 恒 true"的假就绪。
    let grpc_ready = check_grpc_port_simple().await;

    // 就绪检查日志: 仅状态翻转时 info (避免 readinessProbe 每秒刷屏), 稳态 debug。
    let last_ready = LAST_REPORTED_GRPC_READY.load(Ordering::Relaxed);
    if grpc_ready != last_ready {
        info!(
            "🚦 [READY] Readiness transition: http_ready={}, grpc_ready={} ({} → {}), status={}",
            http_ready,
            grpc_ready,
            last_ready,
            grpc_ready,
            if grpc_ready { "ready" } else { "not_ready" }
        );
        LAST_REPORTED_GRPC_READY.store(grpc_ready, Ordering::Relaxed);
    } else {
        debug!(
            "🚦 [READY] Readiness check: http_ready={}, grpc_ready={}, status={}",
            http_ready,
            grpc_ready,
            if grpc_ready { "ready" } else { "not_ready" }
        );
    }

    // 构建健康检查响应
    let health_response =
        shared_types::HealthCheckResponse::new("agent-runner", http_ready, grpc_ready);

    // 根据 gRPC 就绪状态返回不同的 HTTP 状态码
    if grpc_ready {
        (StatusCode::OK, Json(HttpResult::success(health_response)))
    } else {
        // 服务未就绪，返回 HTTP 503 状态码
        // 这样 K8s readinessProbe 会认为 Pod 不是 Ready 的
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(HttpResult {
                code: "SERVICE_NOT_READY".to_string(),
                message: "Service is starting, gRPC not ready".to_string(),
                data: Some(health_response),
                tid: None,
                success: false,
            }),
        )
    }
}

/// 检查本地 gRPC 端口是否可连接
///
/// 使用 TCP 连接检查，快速验证 gRPC 服务是否启动。
/// 仅在启用 grpc-server feature 时使用。
#[cfg(feature = "grpc-server")]
pub async fn check_local_grpc_port() -> bool {
    use tokio::net::TcpStream;
    use tokio::time::{Duration, timeout};
    use tracing::debug;

    let grpc_port = shared_types::GRPC_DEFAULT_PORT;
    let addr = format!("127.0.0.1:{}", grpc_port);

    match timeout(Duration::from_secs(1), TcpStream::connect(&addr)).await {
        Ok(Ok(_)) => {
            debug!("[HEALTH] Local gRPC port check passed: {}", addr);
            true
        }
        Ok(Err(e)) => {
            debug!("[HEALTH] Local gRPC port check failed {}: {}", addr, e);
            false
        }
        Err(_) => {
            debug!("[HEALTH] Local gRPC port check timeout: {}", addr);
            false
        }
    }
}

/// 检查本地 gRPC 端口是否可连接（简化版本，用于轻量健康检查）
///
/// 使用 TCP 连接检查，快速验证 gRPC 服务是否启动。
/// 不检查 feature flag，直接检查端口。
pub async fn check_grpc_port_simple() -> bool {
    use tokio::net::TcpStream;
    use tokio::time::{Duration, timeout};

    let grpc_port = shared_types::GRPC_DEFAULT_PORT;
    let addr = format!("127.0.0.1:{}", grpc_port);

    match timeout(Duration::from_secs(1), TcpStream::connect(&addr)).await {
        Ok(Ok(_)) => true,
        Ok(Err(_)) => false,
        Err(_) => false,
    }
}

/// 构建健康检查响应
///
/// 统一的健康检查响应构建函数，供所有健康检查端点使用。
pub fn build_health_response(
    service_name: &str,
    http_ready: bool,
    grpc_ready: bool,
) -> HttpResult<shared_types::HealthCheckResponse> {
    let health_response =
        shared_types::HealthCheckResponse::new(service_name, http_ready, grpc_ready);

    if grpc_ready {
        HttpResult::success(health_response)
    } else {
        HttpResult {
            code: "SERVICE_NOT_READY".to_string(),
            message: "Service is starting, gRPC not ready".to_string(),
            data: Some(health_response),
            tid: None,
            success: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    // 两个用例都操作 gRPC 端口(50051), 用 tokio async Mutex 串行避免并发 bind 冲突
    // (std::sync::Mutex 持锁跨 await 会触发 clippy await_holding_lock 警告)。
    // tokio::sync::Mutex::new 非 const, 用 LazyLock 包装才能放 static。
    static PORT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[tokio::test]
    async fn ready_check_503_when_grpc_not_listening() {
        let _guard = PORT_LOCK.lock().await;
        // 先确认 50051 当前空闲: bind 成功说明没人在监听
        let probe =
            TcpListener::bind(format!("127.0.0.1:{}", shared_types::GRPC_DEFAULT_PORT)).await;
        if probe.is_err() {
            eprintln!("skip: 50051 已被占用 (可能在跑 agent-runner), 无法测 not-listening");
            return;
        }
        drop(probe); // 留空, ready_check 应连不上 50051 → 503
        let (status, Json(http_result)) = ready_check().await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(!http_result.success);
        assert_eq!(http_result.code, "SERVICE_NOT_READY");
    }

    #[tokio::test]
    async fn ready_check_200_when_grpc_listening() {
        let _guard = PORT_LOCK.lock().await;
        // 占住 50051 模拟 "gRPC 已起" (CI 环境该端口空闲; 本地若跑 agent-runner 会 bind 失败 → skip)
        let listener =
            TcpListener::bind(format!("127.0.0.1:{}", shared_types::GRPC_DEFAULT_PORT)).await;
        let _listener = match listener {
            Ok(l) => l,
            Err(_) => {
                eprintln!("skip: 50051 已被占用 (可能在跑 agent-runner), 无法测 listening-200");
                return;
            }
        };
        let (status, Json(http_result)) = ready_check().await;
        assert_eq!(status, StatusCode::OK);
        assert!(http_result.success);
    }
}
