use std::path::{Path, PathBuf};

use anyhow::Context;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::proxy::admin_probe;
use crate::proxy::compiler::{CompileOutcome, compile_and_validate};

use super::AppState;
use super::envelope;
use super::envelope::HttpResult;

// ── 响应 data DTO（信封载荷；camelCase 与既有调用面保持） ─────────────────────

/// `POST /v1/proxy/validate` 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProxyValidateData {
    /// 配置校验结果（恒 true——无效走错误信封）
    pub valid: bool,
    /// 生效配置落盘路径
    pub effective_config_path: String,
}

/// `POST /v1/proxy/reload` 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProxyReloadData {
    /// 是否已写入生效配置
    pub reloaded: bool,
    /// 是否经 pingap admin config_hash 确认生效（false 不出现——未确认会回切并走错误信封）
    pub verified: bool,
    /// 生效配置内容 hash
    pub config_hash: String,
    /// 当前部署代标识（boot_id）
    pub release_id: String,
    /// 生效配置落盘路径
    pub effective_config_path: String,
}

/// `GET /v1/proxy/status` 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProxyStatusData {
    /// 当前 release ID（idle 态为 null）
    pub release_id: Option<String>,
    /// pingap 模式（idle / manifest 模式名小写）
    pub mode: String,
    /// 生效配置文件是否已落盘
    pub configured: bool,
    /// 生效配置落盘路径
    pub effective_config_path: String,
    /// pingap 版本（idle 态为 null）
    pub pingap_version: Option<String>,
    /// pingap commit（idle 态为 null）
    pub pingap_commit: Option<String>,
}

/// upstream 条目（`GET /v1/proxy/upstreams` data）。
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProxyUpstream {
    /// 服务 ID
    pub service_id: String,
    /// 回环 upstream 地址（127.0.0.1:{port}）
    pub address: String,
    /// 是否启用代理路由（manifest 含 [proxy] 段）
    pub proxied: bool,
}

/// `GET /v1/proxy/upstreams` 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProxyUpstreamsData {
    /// workspace 服务 → 回环 upstream 映射
    pub upstreams: Vec<ProxyUpstream>,
}

#[utoipa::path(
    post,
    path = "/v1/proxy/validate",
    responses(
        (status = 200, body = HttpResult<ProxyValidateData>, description = "Pingap source and plugin validation succeeded"),
        (status = 400, body = HttpResult<String>, description = "Config compile/validation failed (idle: no release)")
    ),
    tag = "Runtime Proxy"
)]
pub(super) async fn validate(State(state): State<AppState>) -> Response {
    match compile(&state).await {
        Ok(outcome) => envelope::ok(
            StatusCode::OK,
            ProxyValidateData {
                valid: true,
                effective_config_path: outcome.config_path.to_string_lossy().to_string(),
            },
        ),
        Err(error) => proxy_error(error),
    }
}

#[utoipa::path(
    post,
    path = "/v1/proxy/reload",
    responses(
        (status = 200, body = HttpResult<ProxyReloadData>, description = "Effective config atomically updated and confirmed live via read-only admin config_hash"),
        (status = 400, body = HttpResult<String>, description = "Compile failed or reload verification timed out (rolled back to previous config)")
    ),
    tag = "Runtime Proxy"
)]
pub(super) async fn reload(State(state): State<AppState>) -> Response {
    // admin 探测未注册（supervisor 尚未启动 pingap）时显式报错，绝不静默跳过确认。
    let Some(endpoint) = admin_probe::admin_endpoint() else {
        return proxy_error(anyhow::anyhow!(
            "pingap admin probe not initialized; supervisor has not started pingap"
        ));
    };

    let target = effective_path(&state);
    // 先读旧生效配置 hash：超时回切后用于 best-effort 二次确认。
    let previous_hash = read_effective_hash(&target).await;

    let outcome = match compile(&state).await {
        Ok(outcome) => outcome,
        Err(error) => return proxy_error(error),
    };
    match admin_probe::wait_for_config_hash(
        endpoint,
        &outcome.expected_hash,
        admin_probe::CONFIRM_BUDGET,
    )
    .await
    {
        Ok(()) => envelope::ok(
            StatusCode::OK,
            ProxyReloadData {
                reloaded: true,
                verified: true,
                config_hash: outcome.expected_hash,
                release_id: state.server.boot_id(),
                effective_config_path: outcome.config_path.to_string_lossy().to_string(),
            },
        ),
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
            proxy_error(
                error.context(
                    "reload took effect verification failed; rolled back to previous config",
                ),
            )
        }
    }
}

#[utoipa::path(
    get,
    path = "/v1/proxy/status",
    responses((status = 200, body = HttpResult<ProxyStatusData>, description = "Proxy release and effective config status")),
    tag = "Runtime Proxy"
)]
pub(super) async fn status(State(state): State<AppState>) -> Response {
    let path = effective_path(&state);
    let data = match state.server.release() {
        Some(release) => ProxyStatusData {
            release_id: Some(release.release_id),
            mode: format!("{:?}", release.pingap.mode).to_ascii_lowercase(),
            configured: path.is_file(),
            effective_config_path: path.to_string_lossy().to_string(),
            pingap_version: Some(release.pingap.version),
            pingap_commit: Some(release.pingap.commit),
        },
        None => ProxyStatusData {
            release_id: None,
            mode: "idle".to_string(),
            configured: false,
            effective_config_path: path.to_string_lossy().to_string(),
            pingap_version: None,
            pingap_commit: None,
        },
    };
    envelope::ok(StatusCode::OK, data)
}

/// 豁免信封（TOML 文本直读端点）：成功/错误保持原 JSON 形态不变。
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
        .map(|content| Json(json!({"releaseId": state.server.boot_id(), "toml": content})))
        .map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "code": "PINGAP_CONFIG_INVALID",
                    "message": format!("read effective Pingap config {}: {error}", path.display()),
                })),
            )
        })
}

#[utoipa::path(
    get,
    path = "/v1/proxy/upstreams",
    responses((status = 200, body = HttpResult<ProxyUpstreamsData>, description = "Workspace service to loopback upstream mapping")),
    tag = "Runtime Proxy"
)]
pub(super) async fn upstreams(State(state): State<AppState>) -> Response {
    let services = state
        .server
        .release()
        .map(|r| r.services)
        .unwrap_or_default();
    let data = ProxyUpstreamsData {
        upstreams: services
            .iter()
            .filter(|service| service.enabled)
            .map(|service| ProxyUpstream {
                service_id: service.service_id.clone(),
                address: format!("127.0.0.1:{}", service.port),
                proxied: service.proxy.is_some(),
            })
            .collect(),
    };
    envelope::ok(StatusCode::OK, data)
}

async fn compile(state: &AppState) -> anyhow::Result<CompileOutcome> {
    let release = state.server.release().ok_or_else(|| {
        anyhow::anyhow!("no release deployed (idle); proxy endpoints unavailable")
    })?;
    compile_and_validate(
        &state.workspace,
        &runtime_root(),
        &state.pingap_bin,
        &release,
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
        .join(state.server.boot_id())
        .join("pingap.toml")
}

fn runtime_root() -> PathBuf {
    std::env::var_os("APP_CLI_PINGAP_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/run/app-cli/pingap".into())
}

fn proxy_error(error: anyhow::Error) -> Response {
    envelope::error(
        StatusCode::BAD_REQUEST,
        "PINGAP_CONFIG_INVALID",
        error.to_string(),
    )
}
