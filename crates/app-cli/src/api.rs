//! 管理 API scaffold（/health、/reload）。
//!
//! 后续扩展：POST /reload 重生成 pingap config（pingap --autoreload 热生效）；
//! GET /services 各子项目+pingap 状态；增删 upstream/location 等。

use axum::{routing::get, routing::post, Json, Router};
use serde_json::json;

/// 启动管理 API（阻塞，由 main.rs 在 tokio::spawn 里跑）。
pub async fn serve(addr: &str) {
    let app = Router::new()
        .route("/health", get(health))
        .route("/reload", post(reload));

    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            tracing::info!("📡 管理 API 监听 http://{addr}");
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("管理 API 异常: {e}");
            }
        }
        Err(e) => tracing::warn!("⚠️  管理 API 绑定 {addr} 失败: {e}"),
    }
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "service": "app-cli" }))
}

async fn reload() -> Json<serde_json::Value> {
    // TODO: 读 workspace manifest → 重生成 pingap config → 写 pingap.toml
    //       → pingap --autoreload 检测文件变化热生效
    Json(json!({ "status": "todo", "message": "dynamic reload not yet implemented" }))
}
