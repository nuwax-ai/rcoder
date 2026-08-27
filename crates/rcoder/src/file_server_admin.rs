//! file-server 分流反向代理管理接口 + CLI 子命令（60000 入口运行时切换）。
//!
//! 两条路径共用同一组 HTTP 端点:
//! - 运维直接调: `GET/POST /api/system/file-server/{status,stop,start,restart}`
//!   (受全局 API key 中间件保护)
//! - CLI 子命令: `rcoder file-server {start,stop,restart,status}` — bootstrap 解析到
//!   子命令时不启动服务, 以 HTTP client 调 localhost 同套端点后退出 (api key 自动
//!   从同源配置读取并填 header, 与服务端闭环)。
//!
//! 开发测试期在 60000 入口切换"分流代理 vs TS 直跑"对比两侧实现:
//! stop 释放 60000 → 容器内 `nuwax-file-server start --env production --port 60000`
//! → 对比完 kill TS → start 代理重占。
//!
//! 历史: 阶段二曾以本组端点管理"内嵌 file-server"（f55f230）；阶段三路由合并后
//! 控制对象换为 file-server-proxy 分流代理, 端点路径保持不变。

use axum::Json;
use serde::Serialize;
use shared_types::HttpResult;

// ── HTTP handlers ─────────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileServerStatus {
    running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<String>,
}

/// 分流反向代理运行状态
#[utoipa::path(
    get,
    path = "/api/system/file-server/status",
    responses((status = 200, description = "file-server 分流代理状态")),
    tag = "system"
)]
pub async fn status() -> Json<HttpResult<FileServerStatus>> {
    let address = file_server_proxy::status().await;
    Json(HttpResult::success(FileServerStatus {
        running: address.is_some(),
        address,
    }))
}

/// 停止并释放 60000 端口（幂等）
#[utoipa::path(
    post,
    path = "/api/system/file-server/stop",
    responses((status = 200, description = "已停止, 60000 已释放")),
    tag = "system"
)]
pub async fn stop() -> Json<HttpResult<serde_json::Value>> {
    match file_server_proxy::stop().await {
        Ok(()) => Json(HttpResult::success(serde_json::json!({
            "message": "file-server proxy stopped, port 60000 released (TS nuwax-file-server can bind now)"
        }))),
        Err(e) => Json(HttpResult::error("FILE_SERVER_STOP_FAILED", &e)),
    }
}

/// 启动分流代理（幂等）
#[utoipa::path(
    post,
    path = "/api/system/file-server/start",
    responses((status = 200, description = "已启动")),
    tag = "system"
)]
pub async fn start() -> Json<HttpResult<serde_json::Value>> {
    match file_server_proxy::try_start().await {
        Ok(address) => Json(HttpResult::success(serde_json::json!({
            "message": "file-server proxy started",
            "address": address,
        }))),
        Err(e) => Json(HttpResult::error("FILE_SERVER_START_FAILED", &e)),
    }
}

/// 停止后重新启动分流代理
#[utoipa::path(
    post,
    path = "/api/system/file-server/restart",
    responses((status = 200, description = "已重启")),
    tag = "system"
)]
pub async fn restart() -> Json<HttpResult<serde_json::Value>> {
    if let Err(e) = file_server_proxy::stop().await {
        return Json(HttpResult::error("FILE_SERVER_STOP_FAILED", &e));
    }
    match file_server_proxy::try_start().await {
        Ok(address) => Json(HttpResult::success(serde_json::json!({
            "message": "file-server proxy restarted",
            "address": address,
        }))),
        Err(e) => Json(HttpResult::error("FILE_SERVER_START_FAILED", &e)),
    }
}

/// 管理路由 (无 state, merge 进 rcoder router; 受全局 API key 中间件保护)。
pub fn admin_routes() -> axum::Router {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/api/system/file-server/status", get(status))
        .route("/api/system/file-server/stop", post(stop))
        .route("/api/system/file-server/start", post(start))
        .route("/api/system/file-server/restart", post(restart))
}

// ── CLI 子命令实现 ────────────────────────────────────────────────────────────

/// 执行管理子命令 (bootstrap 在 `Some(command)` 时调用; 结束后进程退出, 不启动服务)。
/// 通过 HTTP 调 localhost 的同套管理端点, api key 自动从配置读取填充。
///
/// 参数刻意用基础类型 (action: "start"/"stop"/"restart"/"status"): bin 与 lib 双编译
/// 同一 config 模块, 枚举类型名不互通, 字符串化避免类型纠缠。
pub async fn run_cli_command(action: &str, port: u16, api_key: Option<&str>) -> anyhow::Result<()> {
    let base = format!("http://127.0.0.1:{port}/api/system/file-server");

    let client = crate::http_client::shared_client();
    let request = match action {
        "start" => client.post(format!("{base}/start")),
        "stop" => client.post(format!("{base}/stop")),
        "restart" => client.post(format!("{base}/restart")),
        _ => client.get(format!("{base}/status")),
    };
    let request = match api_key.filter(|k| !k.trim().is_empty()) {
        Some(key) => request.header("x-api-key", key),
        None => request,
    };

    let resp = request.send().await.map_err(|e| {
        anyhow::anyhow!(
            "request rcoder (127.0.0.1:{port}) failed: {e}\n\
             hint: rcoder 主服务未运行时无法管理 file-server 分流代理"
        )
    })?;
    let status_code = resp.status();
    let body = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| anyhow::anyhow!("parse response failed (HTTP {status_code}): {e}"))?;
    println!("{body}");
    if !status_code.is_success() {
        anyhow::bail!("HTTP {status_code}");
    }
    // handler 失败时返回 HTTP 200 + body success=false, 必须解析 body 才能判定真实结果
    // (否则 bind 失败等错误场景 CLI 仍 exit 0, 脚本调用方无法感知)
    if body.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
        let msg = body
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown error");
        anyhow::bail!("{msg}");
    }
    Ok(())
}
