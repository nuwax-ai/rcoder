//! app-cli internal management API.

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::Stream;
use serde_json::{Value, json};
use utoipa::OpenApi;

use crate::log::model::{LogQueryRequest, LogQueryResponse, LogSourceInfo};
use crate::log::service::LogService;
use crate::runtime_status::RuntimeStatusService;

mod proxy;

#[derive(OpenApi)]
#[openapi(
    paths(
        query_sources,
        query_logs,
        stream_logs,
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
        crate::log::model::SourceError
    )),
    tags(
        (name = "Runtime Logs", description = "Multi-service declared file logs"),
        (name = "Runtime Proxy", description = "Pingap validation and runtime status")
    )
)]
struct ApiDoc;

#[derive(Clone)]
pub(super) struct AppState {
    logs: LogService,
    workspace: PathBuf,
    pingap_bin: PathBuf,
    release: workspace_manifest::ReleaseLock,
    runtime_status: RuntimeStatusService,
}

pub async fn serve(
    addr: &str,
    workspace: PathBuf,
    log_dir: PathBuf,
    pingap_bin: PathBuf,
    runtime_status: RuntimeStatusService,
) -> Result<()> {
    let release = crate::manifest::read_release_lock(&workspace)?;
    let state = AppState {
        logs: LogService::new(release.clone(), log_dir),
        workspace,
        pingap_bin,
        release,
        runtime_status,
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/openapi.json", get(openapi))
        .route("/v1/logs/sources/query", post(query_sources))
        .route("/v1/logs/query", post(query_logs))
        .route("/v1/logs/stream", post(stream_logs))
        .route("/v1/proxy/validate", post(proxy::validate))
        .route("/v1/proxy/reload", post(proxy::reload))
        .route("/v1/proxy/status", get(proxy::status))
        .route("/v1/proxy/effective-config", get(proxy::effective_config))
        .route("/v1/proxy/upstreams", get(proxy::upstreams))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind app-cli API {addr}"))?;
    tracing::info!("app-cli management API listening on http://{addr}");
    axum::serve(listener, app)
        .await
        .context("serve app-cli management API")
}

async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

async fn health(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    if state.runtime_status.is_ready() {
        (
            StatusCode::OK,
            Json(json!({ "status": "ready", "service": "app-cli" })),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "not_ready", "service": "app-cli" })),
        )
    }
}

#[utoipa::path(
    post,
    path = "/v1/logs/sources/query",
    request_body = LogQueryRequest,
    responses(
        (status = 200, body = Vec<LogSourceInfo>),
        (status = 400, description = "Invalid selector or query")
    ),
    tag = "Runtime Logs"
)]
async fn query_sources(
    State(state): State<AppState>,
    Json(request): Json<LogQueryRequest>,
) -> Result<Json<Vec<LogSourceInfo>>, (StatusCode, Json<Value>)> {
    state.logs.sources(&request).map(Json).map_err(bad_request)
}

#[utoipa::path(
    post,
    path = "/v1/logs/query",
    request_body = LogQueryRequest,
    responses(
        (status = 200, body = LogQueryResponse),
        (status = 400, description = "Invalid selector or query")
    ),
    tag = "Runtime Logs"
)]
async fn query_logs(
    State(state): State<AppState>,
    Json(request): Json<LogQueryRequest>,
) -> Result<Json<LogQueryResponse>, (StatusCode, Json<Value>)> {
    state.logs.query(&request).map(Json).map_err(bad_request)
}

#[utoipa::path(
    post,
    path = "/v1/logs/stream",
    request_body = LogQueryRequest,
    responses(
        (status = 200, description = "SSE events: log, source_error, source_recovered, cursor_reset, checkpoint, heartbeat"),
        (status = 400, description = "Invalid selector or query")
    ),
    tag = "Runtime Logs"
)]
async fn stream_logs(
    State(state): State<AppState>,
    Json(mut request): Json<LogQueryRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, (StatusCode, Json<Value>)> {
    state.logs.sources(&request).map_err(bad_request)?;
    let stream = async_stream::stream! {
        let mut first = true;
        let mut last_checkpoint: Option<String> = None;
        let mut failed_sources: BTreeSet<(String, String)> = BTreeSet::new();
        loop {
            if !first {
                request.tail = None;
            }
            match state.logs.query(&request) {
                Ok(response) => {
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
                                "serviceId": service_id,
                                "sourceId": source_id,
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
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .event(Event::default().event("heartbeat").data("{}")),
    ))
}

fn bad_request(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "code": "INVALID_LOG_QUERY",
            "message": error.to_string(),
        })),
    )
}
