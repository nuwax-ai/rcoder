use std::path::PathBuf;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;
use serde_json::{Value, json};

use crate::proxy::admin_probe;
use crate::proxy::compiler::{CompileOutcome, compile_and_validate};

use super::AppState;
use super::envelope;
use super::envelope::HttpResult;

// ── 响应 data DTO（信封载荷；字段 snake_case，与主仓 userApp Java 契约统一）────

/// `POST /v1/proxy/validate` 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
pub(super) struct ProxyValidateData {
    /// 配置校验结果（恒 true——无效走错误信封）
    pub valid: bool,
    /// 生效配置落盘路径
    pub effective_config_path: String,
}

/// `POST /v1/proxy/reload` 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
pub(super) struct ProxyReloadData {
    /// 是否已写入生效配置
    pub reloaded: bool,
    /// 完整 Applied 回执、进程身份和各监听 HTTP 标记均已确认
    pub verified: bool,
    pub publication_id: String,
    pub process_instance_id: String,
    /// 生效配置内容 hash
    pub config_hash: String,
    /// 当前部署代标识（boot_id）
    pub release_id: String,
    /// 生效配置落盘路径
    pub effective_config_path: String,
}

/// `GET /v1/proxy/status` 响应 data。
#[derive(Serialize, utoipa::ToSchema)]
pub(super) struct ProxyStatusData {
    /// 当前 release ID（idle 态为 null）
    pub release_id: Option<String>,
    /// pingap 模式（idle / manifest 模式名小写）
    pub mode: String,
    /// 生效配置文件是否已落盘
    pub configured: bool,
    /// Confirmed complete graph for this process and publication (including standby).
    pub applied: bool,
    pub publication_id: Option<String>,
    /// 生效配置落盘路径
    pub effective_config_path: String,
    /// pingap 版本（idle 态为 null）
    pub pingap_version: Option<String>,
    /// pingap commit（idle 态为 null）
    pub pingap_commit: Option<String>,
}

/// upstream 条目（`GET /v1/proxy/upstreams` data）。
#[derive(Serialize, utoipa::ToSchema)]
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
pub(super) struct ProxyUpstreamsData {
    /// workspace 服务 → 回环 upstream 映射
    pub upstreams: Vec<ProxyUpstream>,
}

#[utoipa::path(
    post,
    path = "/v1/proxy/validate",
    params(("x-deploy-token" = Option<String>, Header, description = "Required when the runtime control token is configured")),
    responses(
        (status = 403, description = "Runtime control token missing or invalid"),
        (status = 200, body = HttpResult<ProxyValidateData>, description = "Pingap source and plugin validation succeeded"),
        (status = 400, body = HttpResult<String>, description = "Config compile/validation failed (idle: no release)")
    ),
    tag = "Runtime Proxy"
)]
pub(super) async fn validate(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Response {
    if state.server.control_token().is_some()
        && let Err(message) = super::authorize_deploy(&state, &headers)
    {
        return envelope::error(StatusCode::FORBIDDEN, "DEPLOY_FORBIDDEN", message);
    }
    // Validation only creates an immutable candidate; it never changes active.
    // The auxiliary gate freezes the orchestration profile during compilation.
    let mut writer = match state.server.begin_auxiliary_write() {
        Ok(writer) => writer,
        Err(error) => return proxy_error(error),
    };
    let result = compile(&state).await;
    writer.confirm();
    match result {
        Ok(outcome) => envelope::ok(
            StatusCode::OK,
            ProxyValidateData {
                valid: true,
                effective_config_path: outcome.config_path.to_string_lossy().into_owned(),
            },
        ),
        Err(error) => proxy_error(error),
    }
}

#[utoipa::path(
    post,
    path = "/v1/proxy/reload",
    params(("x-deploy-token" = Option<String>, Header, description = "Required when the runtime control token is configured")),
    responses(
        (status = 403, description = "Runtime control token missing or invalid"),
        (status = 200, body = HttpResult<ProxyReloadData>, description = "Active config updated; complete Applied graph, process identity and listener HTTP markers confirmed"),
        (status = 400, body = HttpResult<String>, description = "Compile failed or reload verification timed out (rolled back to previous config)")
    ),
    tag = "Runtime Proxy"
)]
pub(super) async fn reload(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Response {
    if state.server.control_token().is_some()
        && let Err(message) = super::authorize_deploy(&state, &headers)
    {
        return envelope::error(StatusCode::FORBIDDEN, "DEPLOY_FORBIDDEN", message);
    }
    // 初始化恢复期拒绝运行态变更（P1-01：pingap 配置重载与启动恢复竞争）。
    if state.server.initializing() {
        return proxy_error(anyhow::anyhow!(
            "runtime state changes are rejected until startup recovery completes"
        ));
    }
    // admin 探测未注册（supervisor 尚未启动 pingap）时显式报错，绝不静默跳过确认。
    let Some(endpoint) = admin_probe::admin_endpoint() else {
        return proxy_error(anyhow::anyhow!(
            "pingap admin probe not initialized; supervisor has not started pingap"
        ));
    };

    let mut writer = match state.server.begin_auxiliary_write() {
        Ok(writer) => writer,
        Err(error) => return proxy_error(error),
    };
    let target = effective_path(&state);
    let outcome = match compile(&state).await {
        Ok(outcome) => outcome,
        Err(error) => {
            writer.confirm();
            return proxy_error(error);
        }
    };
    let candidate = match tokio::fs::read_to_string(&outcome.config_path).await {
        Ok(content) => content,
        Err(error) => {
            writer.confirm();
            return proxy_error(error.into());
        }
    };
    if let Err(error) = crate::proxy::compiler::validate_hot_reload_compatible(&target, &candidate)
    {
        writer.confirm();
        return proxy_error(error);
    }
    let result = async {
        if let Some(host) = crate::supervisord_host::SupervisordHost::from_env().await? {
            let previous = admin_probe::fetch_apply_status(endpoint)
                .await?
                .current_publication()?;
            host.verify_entry_confirmation(&previous).await?;
            let confirmed = crate::proxy::compiler::publish_confirmed(
                &crate::proxy::compiler::runtime_root(&state.log_dir),
                &target,
                &outcome,
                endpoint,
            )
            .await?;
            host.verify_entry_confirmation(&confirmed)
                .await
                .map_err(|error| {
                    anyhow::Error::from(crate::supervisor::ShutdownUnconfirmed(format!(
                        "proxy physical identity became unknown after reload: {error:#}"
                    )))
                })?;
            Ok(confirmed)
        } else {
            crate::supervisor::resident::reload(&outcome).await
        }
    }
    .await;
    match result {
        Ok(confirmed) => {
            writer.confirm();
            envelope::ok(
                StatusCode::OK,
                ProxyReloadData {
                    reloaded: true,
                    verified: true,
                    config_hash: outcome.expected_hash,
                    publication_id: confirmed.publication_id,
                    process_instance_id: confirmed.instance_id,
                    release_id: state.server.boot_id(),
                    effective_config_path: target.to_string_lossy().into_owned(),
                },
            )
        }
        Err(error) => {
            // A confirmed rollback is an ordinary failed request. Unknown
            // physical/application outcome retains the existing writer fence.
            if !error.is::<crate::supervisor::ShutdownUnconfirmed>() {
                writer.confirm();
            }
            proxy_error(error)
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
    let configured = match tokio::fs::metadata(&path).await {
        Ok(metadata) => metadata.is_file(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return proxy_error(anyhow::anyhow!(
                "observe active proxy configuration: {error}"
            ));
        }
    };
    let expected = crate::proxy::compiler::confirmed_publication();
    let applied = !crate::proxy::compiler::publication_uncertain()
        && match (&expected, admin_probe::admin_endpoint()) {
            (Some((outcome, receipt)), Some(endpoint)) => admin_probe::wait_for_publication(
                endpoint,
                outcome,
                std::time::Duration::from_secs(3),
            )
            .await
            .is_ok_and(|actual| {
                actual.process_id == receipt.process_id && actual.instance_id == receipt.instance_id
            }),
            _ => false,
        };
    let publication_id = expected.map(|(outcome, _)| outcome.publication_id);
    let data = match state.server.release() {
        Some(release) => ProxyStatusData {
            release_id: Some(release.release_id),
            mode: format!("{:?}", release.pingap.mode).to_ascii_lowercase(),
            configured,
            applied,
            publication_id,
            effective_config_path: path.to_string_lossy().to_string(),
            pingap_version: Some(release.pingap.version),
            pingap_commit: Some(release.pingap.commit),
        },
        None => ProxyStatusData {
            release_id: None,
            mode: "idle".to_string(),
            configured,
            applied,
            publication_id,
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
        .map(|content| Json(json!({"release_id": state.server.boot_id(), "toml": content})))
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
    let context = state.server.proxy_context().ok_or_else(|| {
        anyhow::anyhow!("no active orchestration profile; proxy reload unavailable")
    })?;
    compile_and_validate(
        &context.workspace,
        &crate::proxy::compiler::runtime_root(&state.log_dir),
        &state.pingap_bin,
        &release,
        context.dev_profile,
    )
    .await
}

fn effective_path(state: &AppState) -> PathBuf {
    crate::proxy::compiler::active_config_path(&crate::proxy::compiler::runtime_root(
        &state.log_dir,
    ))
}

fn proxy_error(error: anyhow::Error) -> Response {
    envelope::error(
        StatusCode::BAD_REQUEST,
        "PINGAP_CONFIG_INVALID",
        format!("{error:#}"),
    )
}
