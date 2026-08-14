//! 内嵌 file-server 管理接口 + CLI 子命令实现 (迁移期 Rust↔TS 切换)。
//!
//! 两条路径共用同一组 HTTP 端点:
//! - 运维直接调: `GET/POST /api/system/file-server/{status,stop,start,restart}`
//!   (受全局 API key 中间件保护)
//! - CLI 子命令: `rcoder file-server {start,stop,restart,status}` — bootstrap 解析到
//!   子命令时不启动服务, 以 HTTP client 调 localhost 同套端点后退出 (api key 自动
//!   从同源配置读取并填 header, 与服务端闭环)。

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

/// `GET /api/system/file-server/status` — 内嵌 file-server 运行状态。
#[utoipa::path(
    get,
    path = "/api/system/file-server/status",
    responses((status = 200, description = "内嵌 file-server 状态")),
    tag = "system"
)]
pub async fn status() -> Json<HttpResult<FileServerStatus>> {
    let address = crate::file_server_embed::status().await;
    Json(HttpResult::success(FileServerStatus {
        running: address.is_some(),
        address,
    }))
}

/// `POST /api/system/file-server/stop` — 停止并释放端口 (幂等)。
#[utoipa::path(
    post,
    path = "/api/system/file-server/stop",
    responses((status = 200, description = "已停止")),
    tag = "system"
)]
pub async fn stop() -> Json<HttpResult<serde_json::Value>> {
    match crate::file_server_embed::stop().await {
        Ok(()) => Json(HttpResult::success(serde_json::json!({
            "message": "embedded file-server stopped, port released"
        }))),
        Err(e) => Json(HttpResult::error("FILE_SERVER_STOP_FAILED", &e)),
    }
}

/// start/restart 的端口覆盖参数 (`?port=60001`; 缺省走 env/默认)。
#[derive(serde::Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StartPortParam {
    /// file-server 监听端口 (优先级最高, 覆盖 env FILE_SERVER_PORT/PORT)
    pub port: Option<u16>,
}

/// `POST /api/system/file-server/start` — 启动 (幂等; `?port=` 覆盖端口)。
#[utoipa::path(
    post,
    path = "/api/system/file-server/start",
    params(StartPortParam),
    responses((status = 200, description = "已启动")),
    tag = "system"
)]
pub async fn start(
    axum::extract::Query(param): axum::extract::Query<StartPortParam>,
) -> Json<HttpResult<serde_json::Value>> {
    match crate::file_server_embed::try_start(param.port).await {
        Ok(address) => Json(HttpResult::success(serde_json::json!({
            "message": "embedded file-server started",
            "address": address,
        }))),
        Err(e) => Json(HttpResult::error("FILE_SERVER_START_FAILED", &e)),
    }
}

/// `POST /api/system/file-server/restart` — 停止后重新启动 (`?port=` 覆盖端口)。
#[utoipa::path(
    post,
    path = "/api/system/file-server/restart",
    params(StartPortParam),
    responses((status = 200, description = "已重启")),
    tag = "system"
)]
pub async fn restart(
    axum::extract::Query(param): axum::extract::Query<StartPortParam>,
) -> Json<HttpResult<serde_json::Value>> {
    if let Err(e) = crate::file_server_embed::stop().await {
        return Json(HttpResult::error("FILE_SERVER_STOP_FAILED", &e));
    }
    match crate::file_server_embed::try_start(param.port).await {
        Ok(address) => Json(HttpResult::success(serde_json::json!({
            "message": "embedded file-server restarted",
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
/// 同一 config.rs, 枚举类型名不互通, 字符串化避免类型纠缠。
/// `fs_port`: file-server 端口覆盖 (CLI --port, 优先级最高)。
pub async fn run_cli_command(
    action: &str,
    port: u16,
    fs_port: Option<u16>,
    api_key: Option<&str>,
) -> anyhow::Result<()> {
    let base = format!("http://127.0.0.1:{port}/api/system/file-server");

    let client = crate::http_client::shared_client();
    // start/restart 支持 ?port= 覆盖 file-server 监听端口
    let query = fs_port.map(|p| format!("?port={p}")).unwrap_or_default();
    let request = match action {
        "start" => client.post(format!("{base}/start{query}")),
        "stop" => client.post(format!("{base}/stop")),
        "restart" => client.post(format!("{base}/restart{query}")),
        _ => client.get(format!("{base}/status")),
    };
    let request = match api_key.filter(|k| !k.trim().is_empty()) {
        Some(key) => request.header("x-api-key", key),
        None => request,
    };

    let resp = request.send().await.map_err(|e| {
        anyhow::anyhow!(
            "request rcoder (127.0.0.1:{port}) failed: {e}\n\
             hint: rcoder 主服务未运行时无法管理内嵌 file-server"
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
