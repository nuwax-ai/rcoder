use std::path::{Path, PathBuf};

use anyhow::Context;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::proxy::admin_probe;
use crate::proxy::compiler::{CompileOutcome, compile_and_validate};

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
        .map(|outcome| Json(json!({"valid": true, "effectiveConfigPath": outcome.config_path})))
        .map_err(proxy_error)
}

#[utoipa::path(
    post,
    path = "/v1/proxy/reload",
    responses((status = 200, description = "Effective config atomically updated and confirmed live via read-only admin config_hash")),
    tag = "Runtime Proxy"
)]
pub(super) async fn reload(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // admin 探测未注册（supervisor 尚未启动 pingap）时显式报错，绝不静默跳过确认。
    let endpoint = admin_probe::admin_endpoint()
        .ok_or_else(|| {
            anyhow::anyhow!("pingap admin probe not initialized; supervisor has not started pingap")
        })
        .map_err(proxy_error)?;

    let target = effective_path(&state);
    // 先读旧生效配置 hash：超时回切后用于 best-effort 二次确认。
    let previous_hash = read_effective_hash(&target).await;

    let outcome = compile(&state).await.map_err(proxy_error)?;
    match admin_probe::wait_for_config_hash(
        endpoint,
        &outcome.expected_hash,
        admin_probe::CONFIRM_BUDGET,
    )
    .await
    {
        Ok(()) => Ok(Json(json!({
            "reloaded": true,
            "verified": true,
            "configHash": outcome.expected_hash,
            "releaseId": state.release.release_id,
            "effectiveConfigPath": outcome.config_path,
        }))),
        Err(error) => {
            // 超时/不匹配 → 回切 pingap.toml.prev。语义说明：basic/storages/server addr
            // 类变更在 --autoreload 下本就热更不生效 → hash 永不匹配 → 超时+回切是
            // 正确的 fail-safe（宁可回退也不让未确认的新配置留在生效位）。
            match rollback_to_previous(&target).await {
                Ok(()) => warn!(
                    "⚠️  reload verification failed, rolled back to previous config: {error:#}"
                ),
                Err(rollback_error) => warn!(
                    "⚠️  reload verification failed and rollback failed too: \
                     verification={error:#} rollback={rollback_error:#}"
                ),
            }
            // best-effort 二次确认旧 hash 重新生效；失败仅 warn，不影响错误返回。
            if let Some(previous_hash) = previous_hash {
                match admin_probe::wait_for_config_hash(
                    endpoint,
                    &previous_hash,
                    admin_probe::ROLLBACK_CONFIRM_BUDGET,
                )
                .await
                {
                    Ok(()) => info!("✅ rollback confirmed: previous config_hash live again"),
                    Err(verify_error) => {
                        warn!("⚠️  rollback verification failed (non-fatal): {verify_error:#}")
                    }
                }
            }
            Err(proxy_error(error.context(
                "reload took effect verification failed; rolled back to previous config",
            )))
        }
    }
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

async fn compile(state: &AppState) -> anyhow::Result<CompileOutcome> {
    compile_and_validate(
        &state.workspace,
        &runtime_root(),
        &state.pingap_bin,
        &state.release,
    )
    .await
}

/// 读当前生效 TOML 并计算其 config_hash（best-effort，失败返 None）。
async fn read_effective_hash(path: &Path) -> Option<String> {
    let bytes = tokio::fs::read(path).await.ok()?;
    let config = pingap_config::PingapConfig::new(&bytes, true).ok()?;
    config.hash().ok()
}

/// 回切：把 pingap.toml.prev 还原为 pingap.toml（autoreload 会自动感知文件变更）。
async fn rollback_to_previous(target: &Path) -> anyhow::Result<()> {
    let backup = target.with_file_name("pingap.toml.prev");
    if !tokio::fs::try_exists(&backup).await.unwrap_or(false) {
        anyhow::bail!(
            "no previous config backup {} to roll back to",
            backup.display()
        );
    }
    tokio::fs::rename(&backup, target)
        .await
        .with_context(|| format!("roll back Pingap config {}", target.display()))
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
