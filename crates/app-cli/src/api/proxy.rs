use std::path::PathBuf;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::proxy::compiler::compile_and_validate;

use super::AppState;

#[utoipa::path(
    post,
    path = "/v1/proxy/validate",
    responses((status = 200, description = "Pingap source and plugin validation succeeded")),
    tag = "Runtime Proxy"
)]
pub(super) async fn validate(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    compile(&state)
        .await
        .map(|path| Json(json!({"valid": true, "effectiveConfigPath": path})))
        .map_err(proxy_error)
}

#[utoipa::path(
    post,
    path = "/v1/proxy/reload",
    responses((status = 200, description = "Effective config atomically updated for autoreload")),
    tag = "Runtime Proxy"
)]
pub(super) async fn reload(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    compile(&state)
        .await
        .map(|path| {
            Json(json!({
                "reloaded": true,
                "releaseId": state.release.release_id,
                "effectiveConfigPath": path,
            }))
        })
        .map_err(proxy_error)
}

#[utoipa::path(
    get,
    path = "/v1/proxy/status",
    responses((status = 200, description = "Proxy release and effective config status")),
    tag = "Runtime Proxy"
)]
pub(super) async fn status(State(state): State<AppState>) -> Json<Value> {
    let path = effective_path(&state);
    Json(json!({
        "releaseId": state.release.release_id,
        "mode": format!("{:?}", state.release.pingap.mode).to_ascii_lowercase(),
        "configured": path.is_file(),
        "effectiveConfigPath": path,
        "pingapVersion": state.release.pingap.version,
        "pingapCommit": state.release.pingap.commit,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/proxy/effective-config",
    responses((status = 200, description = "Effective Pingap TOML")),
    tag = "Runtime Proxy"
)]
pub(super) async fn effective_config(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let path = effective_path(&state);
    tokio::fs::read_to_string(&path)
        .await
        .map(|content| Json(json!({"releaseId": state.release.release_id, "toml": content})))
        .map_err(|error| {
            proxy_error(anyhow::anyhow!(
                "read effective Pingap config {}: {error}",
                path.display()
            ))
        })
}

#[utoipa::path(
    get,
    path = "/v1/proxy/upstreams",
    responses((status = 200, description = "Workspace service to loopback upstream mapping")),
    tag = "Runtime Proxy"
)]
pub(super) async fn upstreams(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "upstreams": state.release.services.iter().filter(|service| service.enabled).map(|service| {
            json!({
                "serviceId": service.service_id,
                "address": format!("127.0.0.1:{}", service.port),
                "proxied": service.proxy.is_some(),
            })
        }).collect::<Vec<_>>()
    }))
}

async fn compile(state: &AppState) -> anyhow::Result<PathBuf> {
    compile_and_validate(
        &state.workspace,
        &runtime_root(),
        &state.pingap_bin,
        &state.release,
    )
    .await
}

fn effective_path(state: &AppState) -> PathBuf {
    runtime_root()
        .join(&state.release.release_id)
        .join("pingap.toml")
}

fn runtime_root() -> PathBuf {
    std::env::var_os("APP_CLI_PINGAP_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/run/app-cli/pingap".into())
}

fn proxy_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "code": "PINGAP_CONFIG_INVALID",
            "message": error.to_string(),
        })),
    )
}
