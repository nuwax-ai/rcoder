//! app-cli internal management API.
//!
//! server 形态下 AppState 持 `Arc<ServerState>`：release/phase 随部署动态变化
//! （idle 态全部日志/代理端点降级，探针按状态机应答）；legacy 直跑形态由
//! main 构造等价 ServerState（读 lock 后直接 Running 相位），路由面零分叉。
//!
//! 响应形态：JSON 端点统一 [`envelope::HttpResult`] 信封
//! （`{code, message, data, tid, success}`，保留语义状态码）；豁免 = SSE
//! （`/v1/logs/stream`）、kubelet 探针（`/health`、`/ready`）、TOML 文本
//! （`/v1/proxy/effective-config`）。

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::Stream;
use serde::Deserialize;
use serde::Serialize;
use serde_json::{Value, json};
use utoipa::OpenApi;

use crate::log::model::{LogQueryRequest, LogQueryResponse, LogSourceInfo};
use crate::log::service::LogService;
use crate::server::{DeployRequest, ServerState};

mod envelope;
mod proxy;

use envelope::ApiJson;

#[derive(OpenApi)]
#[openapi(
    paths(
        query_sources,
        query_logs,
        stream_logs,
        submit_deploy,
        deploy_status,
        proxy::validate,
        proxy::reload,
        proxy::status,
        proxy::effective_config,
        proxy::upstreams
    ),
    components(schemas(
        LogQueryRequest,
        crate::log::model::LogSelector,
        LogQueryResponse,
        LogSourceInfo,
        crate::log::model::LogRecord,
        crate::log::model::SourceError,
        DeployBody,
        DeployAcceptedData,
        crate::server::DeployStatus,
    )),
    tags(
        (name = "Runtime Logs", description = "Multi-service declared file logs"),
        (name = "Runtime Proxy", description = "Pingap validation and runtime status"),
        (name = "Runtime Deploy", description = "Hot deploy without pod replacement")
    )
)]
struct ApiDoc;

#[derive(Clone)]
pub(super) struct AppState {
    server: Arc<ServerState>,
    workspace: PathBuf,
    pingap_bin: PathBuf,
    log_dir: PathBuf,
}

impl AppState {
    /// 按当前 release/部署代/引擎布局构造日志服务（idle → 空服务集）。
    fn logs(&self) -> LogService {
        match self.server.release() {
            Some(release) => LogService::with_boot_id(
                release,
                self.log_dir.clone(),
                self.server.boot_id(),
                self.server.log_layout(),
            ),
            None => LogService::idle(self.log_dir.clone()),
        }
    }
}

pub async fn serve(
    addr: &str,
    workspace: PathBuf,
    log_dir: PathBuf,
    pingap_bin: PathBuf,
    server: Arc<ServerState>,
) -> Result<()> {
    let state = AppState {
        server,
        workspace,
        pingap_bin,
        log_dir,
    };
    let app = api_router(state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind app-cli API {addr}"))?;
    tracing::info!("app-cli management API listening on http://{addr}");
    axum::serve(listener, app)
        .await
        .context("serve app-cli management API")
}

/// 管理 API 路由（serve 与测试共用）。
fn api_router(state: AppState) -> Router {
    Router::new()
        // liveness 探针:app-cli 进程能响应即活(永远 200,不依赖任何后端/部署态)。
        // 后端 app 有 bug 起不来时,liveness 不杀容器,用户可 kubectl exec 进去排查。
        .route("/health", get(health))
        // readiness 探针:状态机驱动——Idle=基础设施就绪(PG/ttyd/dbx supervisord
        // 自治,空容器可用);Running=编排完成/bridge 桥接;Deploying/Orchestrating/
        // Failed=503(摘流不杀)。
        .route("/ready", get(ready))
        .route("/openapi.json", get(openapi))
        .route("/v1/logs/sources/query", post(query_sources))
        .route("/v1/logs/query", post(query_logs))
        .route("/v1/logs/stream", post(stream_logs))
        .route("/v1/proxy/validate", post(proxy::validate))
        .route("/v1/proxy/reload", post(proxy::reload))
        .route("/v1/proxy/status", get(proxy::status))
        .route("/v1/proxy/effective-config", get(proxy::effective_config))
        .route("/v1/proxy/upstreams", get(proxy::upstreams))
        .route("/v1/deploy", post(submit_deploy))
        .route("/v1/deploy/status", get(deploy_status))
        .with_state(state)
}

async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

/// `/health` — liveness 探针:app-cli 进程活就 200(能响应即活)。不强依赖后端 app。
async fn health() -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({ "status": "alive", "service": "app-cli" })),
    )
}

/// `/ready` — readiness 探针:状态机驱动（见路由注册处注释）。
async fn ready(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let phase = state.server.phase();
    if state.server.readiness_ok() {
        (
            StatusCode::OK,
            Json(json!({ "status": "ready", "phase": phase.as_str() })),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "not_ready", "phase": phase.as_str() })),
        )
    }
}

/// `POST /v1/deploy` 请求体（热部署）。
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(super) struct DeployBody {
    /// 制品包下载 URL（workspace 整体包 zip）。
    pub url: String,
    /// 发布版本标记（幂等键）。
    pub release_id: String,
    /// 制品 sha256（64 位十六进制小写，可选——给出则下载后校验）。
    #[serde(default)]
    pub sha256: Option<String>,
}

/// `POST /v1/deploy` 受理成功响应 data。
#[derive(Serialize, utoipa::ToSchema)]
pub(super) struct DeployAcceptedData {
    /// 受理状态（"accepted"）
    pub status: String,
    /// 轮询部署进度的端点路径
    pub poll: String,
}

/// `POST /v1/deploy` — 热部署受理（不换 Pod：PG/ttyd/dbx 不断连，仅应用服务切换）。
///
/// 鉴权：请求头 `X-Deploy-Token` 必须等于容器 env `APP_CLI_DEPLOY_TOKEN`
///（未设置该 env = 端点禁用，403——安全默认）。进行中相位（deploying/
/// orchestrating）拒绝 409；受理后由 server 主循环执行（下载成功才停旧服务）。
#[utoipa::path(
    post,
    path = "/v1/deploy",
    request_body = DeployBody,
    params(("X-Deploy-Token" = String, Header, description = "Deploy token (container env APP_CLI_DEPLOY_TOKEN)")),
    responses(
        (status = 202, body = envelope::HttpResult<DeployAcceptedData>, description = "Deploy accepted; poll /v1/deploy/status"),
        (status = 403, body = envelope::HttpResult<String>, description = "Token missing/mismatch or endpoint disabled"),
        (status = 409, body = envelope::HttpResult<String>, description = "Deploy already in progress"),
        (status = 400, body = envelope::HttpResult<String>, description = "Invalid body (sha256 shape etc.)")
    ),
    tag = "Runtime Deploy"
)]
async fn submit_deploy(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    ApiJson(body): ApiJson<DeployBody>,
) -> Response {
    if let Err(message) = authorize_deploy(&headers) {
        return envelope::error(StatusCode::FORBIDDEN, "DEPLOY_FORBIDDEN", message);
    }
    if let Some(sha) = body.sha256.as_deref()
        && (sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return envelope::error(
            StatusCode::BAD_REQUEST,
            "INVALID_SHA256",
            "sha256 must be 64 hex characters",
        );
    }
    match state.server.try_accept_deploy(DeployRequest {
        url: body.url,
        release_id: body.release_id,
        sha256: body.sha256,
    }) {
        Ok(()) => envelope::ok(
            StatusCode::ACCEPTED,
            DeployAcceptedData {
                status: "accepted".to_string(),
                poll: "/v1/deploy/status".to_string(),
            },
        ),
        Err(message) => envelope::error(StatusCode::CONFLICT, "DEPLOY_IN_PROGRESS", message),
    }
}

/// 校验部署令牌：env 未配置 = 端点禁用（安全默认——:3010 在 pod 网络内可达）。
fn authorize_deploy(headers: &axum::http::HeaderMap) -> Result<(), String> {
    let expected = std::env::var("APP_CLI_DEPLOY_TOKEN")
        .ok()
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| "deploy endpoint disabled (APP_CLI_DEPLOY_TOKEN not set)".to_string())?;
    let provided = headers
        .get("x-deploy-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if provided == expected {
        Ok(())
    } else {
        Err("deploy token mismatch".to_string())
    }
}

/// `GET /v1/deploy/status` — 部署进度快照（phase/release_id/error）。
#[utoipa::path(
    get,
    path = "/v1/deploy/status",
    responses(
        (status = 200, body = envelope::HttpResult<crate::server::DeployStatus>, description = "Current deploy/server phase"),
    ),
    tag = "Runtime Deploy"
)]
async fn deploy_status(State(state): State<AppState>) -> Response {
    envelope::ok(StatusCode::OK, state.server.deploy_status())
}

#[utoipa::path(
    post,
    path = "/v1/logs/sources/query",
    request_body = LogQueryRequest,
    responses(
        (status = 200, body = envelope::HttpResult<Vec<LogSourceInfo>>, description = "Declared log sources and matched files"),
        (status = 400, body = envelope::HttpResult<String>, description = "Invalid selector or query")
    ),
    tag = "Runtime Logs"
)]
async fn query_sources(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<LogQueryRequest>,
) -> Response {
    match state.logs().sources(request).await {
        Ok(data) => envelope::ok(StatusCode::OK, data),
        Err(error) => bad_request(error),
    }
}

#[utoipa::path(
    post,
    path = "/v1/logs/query",
    request_body = LogQueryRequest,
    responses(
        (status = 200, body = envelope::HttpResult<LogQueryResponse>, description = "Multi-service log snapshot with checkpoint cursor"),
        (status = 400, body = envelope::HttpResult<String>, description = "Invalid selector or query")
    ),
    tag = "Runtime Logs"
)]
async fn query_logs(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<LogQueryRequest>,
) -> Response {
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel_on_drop = CancelOnDrop(cancelled.clone());
    match state.logs().query_with_cancel(request, cancelled).await {
        Ok(data) => envelope::ok(StatusCode::OK, data),
        Err(error) => bad_request(error),
    }
}

#[utoipa::path(
    post,
    path = "/v1/logs/stream",
    request_body = LogQueryRequest,
    responses(
        (status = 200, description = "SSE events: log, source_error, source_recovered, cursor_reset, checkpoint, heartbeat"),
        (status = 400, body = envelope::HttpResult<String>, description = "Invalid selector or query")
    ),
    tag = "Runtime Logs"
)]
async fn stream_logs(
    State(state): State<AppState>,
    ApiJson(mut request): ApiJson<LogQueryRequest>,
) -> Response {
    // 参数错在建流前 → 信封 400（流本身是 SSE，豁免信封）
    if let Err(error) = state.logs().sources(request.clone()).await {
        return bad_request(error);
    }
    // 注意：流生命周期内用同一份 LogService 快照（游标/源清单不因并发部署换代
    // 而漂移；换代后 cursor boot_id 不匹配 → cursor_reset 事件，客户端自然重放）
    let logs = state.logs();
    let stream = async_stream::stream! {
        let cancelled = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(cancelled.clone());
        let mut first = true;
        let mut last_checkpoint: Option<String> = None;
        let mut failed_sources: BTreeSet<(String, String)> = BTreeSet::new();
        loop {
            if !first {
                request.tail = None;
            }
            match logs.query_with_cancel(request.clone(), cancelled.clone()).await {
                Ok(response) => {
                    if response.cursor_reset {
                        yield Ok(Event::default()
                            .event("cursor_reset")
                            .data(json!({"message": "cursor belongs to a previous deploy generation"}).to_string()));
                    }
                    for record in response.logs {
                        if let Ok(data) = serde_json::to_string(&record) {
                            yield Ok(Event::default().event("log").data(data));
                        }
                    }
                    let mut current_failures = BTreeSet::new();
                    for error in response.source_errors {
                        let key = (error.service_id.clone(), error.source_id.clone());
                        current_failures.insert(key.clone());
                        if !failed_sources.contains(&key)
                            && let Ok(data) = serde_json::to_string(&error)
                        {
                            yield Ok(Event::default().event("source_error").data(data));
                        }
                    }
                    for (service_id, source_id) in failed_sources.difference(&current_failures) {
                        yield Ok(Event::default().event("source_recovered").data(
                            json!({
                                "service_id": service_id,
                                "source_id": source_id,
                            })
                            .to_string(),
                        ));
                    }
                    failed_sources = current_failures;
                    request.cursor = Some(response.cursor.clone());
                    if last_checkpoint.as_deref() != Some(response.cursor.as_str()) {
                        last_checkpoint = Some(response.cursor.clone());
                        yield Ok(Event::default().event("checkpoint").data(response.cursor));
                    }
                }
                Err(error) => {
                    yield Ok(Event::default()
                        .event("cursor_reset")
                        .data(json!({"message": error.to_string()}).to_string()));
                    request.cursor = None;
                    last_checkpoint = None;
                }
            }
            first = false;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    // handler 返回 Response 后，async_stream yield 的 Result 错误类型失去
    // 签名锚点（改造前由 `Result<Sse<impl Stream<..>>, _>` 签名推断），此处
    // 显式固定。
    let stream: std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(stream);
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .event(Event::default().event("heartbeat").data("{}")),
        )
        .into_response()
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn bad_request(error: anyhow::Error) -> Response {
    envelope::error(
        StatusCode::BAD_REQUEST,
        "INVALID_LOG_QUERY",
        error.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_status::RuntimeStatusService;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::util::ServiceExt;

    /// idle 态 AppState（无 release）：日志空集、proxy 端点走 idle 降级错误。
    fn test_state() -> AppState {
        AppState {
            server: Arc::new(ServerState::new(RuntimeStatusService::default())),
            workspace: PathBuf::from("/nonexistent"),
            pingap_bin: PathBuf::from("/bin/true"),
            log_dir: tempfile::tempdir().unwrap().keep(),
        }
    }

    async fn call(
        state: &AppState,
        method: &str,
        uri: &str,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let response = api_router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).expect("response body is JSON");
        (status, value)
    }

    /// 成功信封：恒 5 键、data 载荷透出（idle 空 selector → 空日志快照）。
    #[tokio::test]
    async fn logs_query_success_envelope() {
        let state = test_state();
        let (status, body) = call(&state, "POST", "/v1/logs/query", r#"{"selectors": []}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "0000");
        assert_eq!(body["success"], true);
        assert_eq!(body["tid"], serde_json::Value::Null);
        assert!(body["data"]["logs"].as_array().unwrap().is_empty());
        assert_eq!(body["data"].as_object().unwrap().len(), 4);
    }

    /// 业务错误信封：INVALID_LOG_QUERY + data 恒 null（未知 service selector）。
    #[tokio::test]
    async fn logs_query_invalid_selector_error_envelope() {
        let state = test_state();
        let (status, body) = call(
            &state,
            "POST",
            "/v1/logs/query",
            r#"{"selectors": [{"service_id": "nope"}]}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "INVALID_LOG_QUERY");
        assert_eq!(body["success"], false);
        assert_eq!(body["data"], serde_json::Value::Null);
        assert!(body["message"].as_str().unwrap().contains("nope"));
    }

    /// 请求体解析失败也回信封（不漏 axum 默认纯文本 400）。
    #[tokio::test]
    async fn invalid_json_body_rejected_with_envelope() {
        let state = test_state();
        let (status, body) = call(&state, "POST", "/v1/logs/query", "not-json").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "INVALID_BODY");
        assert_eq!(body["success"], false);
    }

    /// 无令牌 → 403 信封（env 未配置=端点禁用 / 已配置但不匹配，两路都是
    /// DEPLOY_FORBIDDEN，测试不依赖宿主机 env）。
    #[tokio::test]
    async fn deploy_without_token_403_envelope() {
        let state = test_state();
        let (status, body) = call(
            &state,
            "POST",
            "/v1/deploy",
            r#"{"url": "http://x/p.zip", "release_id": "rel-1"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "DEPLOY_FORBIDDEN");
        assert_eq!(body["data"], serde_json::Value::Null);
    }

    /// deploy/status 成功信封：data.phase 为状态机相位（初始 idle）。
    #[tokio::test]
    async fn deploy_status_envelope() {
        let state = test_state();
        let (status, body) = call(&state, "GET", "/v1/deploy/status", "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "0000");
        assert_eq!(body["data"]["phase"], "idle");
        assert_eq!(body["success"], true);
    }

    /// proxy/status idle 信封：data.mode=idle、release_id=null。
    #[tokio::test]
    async fn proxy_status_idle_envelope() {
        let state = test_state();
        let (status, body) = call(&state, "GET", "/v1/proxy/status", "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "0000");
        assert_eq!(body["data"]["mode"], "idle");
        assert_eq!(body["data"]["release_id"], serde_json::Value::Null);
    }

    /// proxy/validate idle 错误信封：PINGAP_CONFIG_INVALID + data=null。
    #[tokio::test]
    async fn proxy_validate_idle_error_envelope() {
        let state = test_state();
        let (status, body) = call(&state, "POST", "/v1/proxy/validate", "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "PINGAP_CONFIG_INVALID");
        assert_eq!(body["data"], serde_json::Value::Null);
    }
}
