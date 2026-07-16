//! 健康检查端点。
//!
//! 契约: 返回 HTTP 200 + `{ "status": "ok", ... }`。
//! `start-services.sh` 周期 curl 此端点, 连续失败会 kill + 重启 file-server,
//! 因此 `status: "ok"` 字段不可缺 (对齐 nuwax `routes/router.js`)。

use axum::Json;
use serde_json::{Value, json};

/// `GET /health`
pub async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "file-server",
    }))
}
