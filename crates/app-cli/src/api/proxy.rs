use std::path::PathBuf;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shared_types::{ProxyConfirmation, ProxyPhase, ProxyReloadOperation, ProxyReloadState};

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
#[derive(Clone, Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub(super) struct ProxyReloadData {
    /// 是否已写入生效配置
    pub reloaded: bool,
    /// Deprecated: official runtime does not issue private Applied evidence.
    #[schema(deprecated)]
    pub verified: bool,
    pub publication_id: String,
    /// Deprecated: official runtime returns an empty string; use confirmation.
    #[schema(deprecated)]
    pub process_instance_id: String,
    /// 生效配置内容 hash
    pub config_hash: String,
    /// SHA256 of the complete immutable candidate graph.
    pub config_digest: String,
    /// 当前部署代标识（boot_id）
    pub release_id: String,
    /// 生效配置落盘路径
    pub effective_config_path: String,
    pub operation_id: String,
    pub state: ProxyReloadState,
    pub replayed: bool,
    pub confirmation: Option<ProxyConfirmation>,
}

impl ProxyReloadData {
    pub(super) fn pending(operation: &ProxyReloadOperation) -> Self {
        Self {
            reloaded: false,
            verified: false,
            publication_id: String::new(),
            process_instance_id: String::new(),
            config_hash: String::new(),
            config_digest: String::new(),
            release_id: String::new(),
            effective_config_path: String::new(),
            operation_id: operation.operation_id.clone(),
            state: operation.state,
            replayed: false,
            confirmation: None,
        }
    }
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
    /// Deprecated: official runtime does not provide private Applied evidence.
    #[schema(deprecated)]
    pub applied: bool,
    pub publication_id: Option<String>,
    /// 生效配置落盘路径
    pub effective_config_path: String,
    /// pingap 版本（idle 态为 null）
    pub pingap_version: Option<String>,
    /// pingap commit（idle 态为 null）
    pub pingap_commit: Option<String>,
    /// Confirmation owned by app-cli and rechecked over official HTTP APIs.
    pub confirmation: Option<ProxyConfirmation>,
    /// Requested reload identity, including terminal results.
    pub operation: Option<ProxyReloadOperation>,
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
    let validation_id = uuid::Uuid::new_v4().to_string();
    let mut writer = match state.server.begin_proxy_write(&validation_id).await {
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
    params(
        ("x-deploy-token" = Option<String>, Header, description = "Required when the runtime control token is configured"),
        ("X-Request-ID" = Option<String>, Header, description = "1..128 printable ASCII characters; repeat the same ID to replay its frozen request and result")
    ),
    responses(
        (status = 403, description = "Runtime control token missing or invalid"),
        (status = 200, body = HttpResult<ProxyReloadData>, description = "Official proxy process replacement and HTTP verification succeeded, or original successful result replayed"),
        (status = 202, body = HttpResult<ProxyReloadData>, description = "Same reload request remains in progress"),
        (status = 400, body = HttpResult<ProxyReloadOperation>, description = "Candidate validation or replacement failed; data preserves this request identity"),
        (status = 409, body = HttpResult<ProxyReloadOperation>, description = "Different request metadata or an active writer prevents admission; no queue"),
        (status = 503, body = HttpResult<ProxyReloadOperation>, description = "Cleanup or reload outcome remains unconfirmed")
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
    let id = match super::proxy_reload::request_id(&headers) {
        Ok(id) => id,
        Err(error) => return proxy_error(error),
    };
    if state.server.initializing() {
        return reload_rejection(
            &id,
            StatusCode::BAD_REQUEST,
            "PINGAP_CONFIG_INVALID",
            &anyhow::anyhow!("runtime state changes are rejected until startup recovery completes"),
        );
    }
    let metadata = match super::proxy_reload::RequestMetadata::capture(&state) {
        Ok(metadata) => metadata,
        Err(error) => {
            return reload_rejection(
                &id,
                StatusCode::BAD_REQUEST,
                "PINGAP_CONFIG_INVALID",
                &error,
            );
        }
    };
    let journal = super::proxy_reload::root(&state);
    match super::proxy_reload::read(&journal, &id) {
        Ok(Some(record)) => {
            if let Err(error) = record.check_metadata(&metadata) {
                return reload_rejection(
                    &id,
                    StatusCode::CONFLICT,
                    "ERR_OPERATION_ID_CONFLICT",
                    &error,
                );
            }
            return record.response(&state, true);
        }
        Ok(None) => {}
        Err(error) => {
            return reload_rejection(
                &id,
                StatusCode::SERVICE_UNAVAILABLE,
                "ERR_RECOVERY_REQUIRED",
                &error,
            );
        }
    }
    let gate = match super::proxy_reload::gate() {
        Ok(gate) => gate,
        Err(error) => {
            return reload_rejection(
                &id,
                StatusCode::CONFLICT,
                "ERR_OPERATION_IN_PROGRESS",
                &error,
            );
        }
    };
    // Recheck after taking the nonqueueing gate: a concurrent same-ID request
    // may have completed between the read and this admission boundary.
    match super::proxy_reload::read(&journal, &id) {
        Ok(Some(record)) => {
            if let Err(error) = record.check_metadata(&metadata) {
                return reload_rejection(
                    &id,
                    StatusCode::CONFLICT,
                    "ERR_OPERATION_ID_CONFLICT",
                    &error,
                );
            }
            return record.response(&state, true);
        }
        Ok(None) => {}
        Err(error) => {
            return reload_rejection(
                &id,
                StatusCode::SERVICE_UNAVAILABLE,
                "ERR_RECOVERY_REQUIRED",
                &error,
            );
        }
    }
    let response_id = id.clone();
    let task = process_utils::command_context::spawn_scoped(async move {
        let _gate = gate;
        execute_reload(state, id, metadata).await
    });
    // Dropping an HTTP handler only detaches this owned task. An already sent
    // supervisord write must still settle before its gate or owner is released.
    match task.await {
        Ok(response) => response,
        Err(error) => reload_rejection(
            &response_id,
            StatusCode::SERVICE_UNAVAILABLE,
            "ERR_RECOVERY_REQUIRED",
            &anyhow::Error::new(error),
        ),
    }
}

async fn execute_reload(
    state: AppState,
    id: String,
    metadata: super::proxy_reload::RequestMetadata,
) -> Response {
    if state.server.current_runtime_operation().is_some() {
        return reload_rejection(
            &id,
            StatusCode::CONFLICT,
            "ERR_OPERATION_IN_PROGRESS",
            &anyhow::anyhow!("a business runtime operation is in progress"),
        );
    }
    let mut writer = match state.server.begin_proxy_write(&id).await {
        Ok(writer) => writer,
        Err(error) => {
            return reload_rejection(
                &id,
                StatusCode::CONFLICT,
                "ERR_OPERATION_IN_PROGRESS",
                &error,
            );
        }
    };
    match super::proxy_reload::RequestMetadata::capture(&state) {
        Ok(actual) if actual == metadata => {}
        Ok(_) => {
            writer.confirm();
            return reload_rejection(
                &id,
                StatusCode::CONFLICT,
                "ERR_OPERATION_ID_CONFLICT",
                &anyhow::anyhow!("proxy profile changed before admission"),
            );
        }
        Err(error) => {
            writer.confirm();
            return reload_rejection(
                &id,
                StatusCode::BAD_REQUEST,
                "PINGAP_CONFIG_INVALID",
                &error,
            );
        }
    }
    let mut claim = match super::proxy_reload::Claim::admit(&state, id.clone(), metadata.clone()) {
        Ok(claim) => claim,
        Err(error) => {
            // Admission and head persistence happen before any proxy command.
            // A head-write failure may already have a truthful Failed record;
            // replay that exact result instead of inventing unknown physics.
            writer.confirm();
            if let Ok(Some(record)) =
                super::proxy_reload::read(&super::proxy_reload::root(&state), &id)
                && record.check_metadata(&metadata).is_ok()
                && record.operation.state == ProxyReloadState::Failed
            {
                return record.response(&state, false);
            }
            return reload_rejection(
                &id,
                StatusCode::SERVICE_UNAVAILABLE,
                "ERR_RECOVERY_REQUIRED",
                &error,
            );
        }
    };
    let target = effective_path(&state);
    let result = async {
        let mut outcome = compile(&state).await?;
        anyhow::ensure!(
            !writer.cancellation().is_cancelled(),
            "proxy reload cancelled before runtime dispatch"
        );
        anyhow::ensure!(
            super::proxy_reload::RequestMetadata::capture(&state)? == metadata,
            "proxy reload profile or deployment identity changed during preparation"
        );
        outcome.operation_id = Some(id.clone());
        let mut data = ProxyReloadData {
            reloaded: false,
            verified: false,
            config_hash: outcome.expected_hash.clone(),
            config_digest: outcome.config_digest.clone(),
            publication_id: outcome.publication_id.clone(),
            process_instance_id: String::new(),
            release_id: state.server.boot_id(),
            effective_config_path: target.to_string_lossy().into_owned(),
            operation_id: id.clone(),
            state: ProxyReloadState::Running,
            replayed: false,
            confirmation: None,
        };
        claim.running(data.clone())?;
        if let Some(host) = crate::supervisord_host::SupervisordHost::from_env().await? {
            host.replace_official_with_cancel(
                &crate::proxy::compiler::runtime_root(&state.log_dir),
                &outcome,
                tokio::time::Instant::now() + admin_probe::CONFIRM_BUDGET,
                Some(&writer.cancellation()),
            )
            .await?;
        } else {
            crate::supervisor::resident::reload_with_cancel(&outcome, &writer.cancellation())
                .await?;
        }
        anyhow::ensure!(
            !writer.cancellation().is_cancelled(),
            "proxy reload cancelled before success publication"
        );
        anyhow::ensure!(
            super::proxy_reload::RequestMetadata::capture(&state)? == metadata,
            "proxy reload profile or deployment identity changed before success publication"
        );
        data.reloaded = true;
        data.state = ProxyReloadState::Succeeded;
        data.confirmation = crate::proxy::compiler::official_confirmation();
        anyhow::ensure!(
            data.confirmation
                .as_ref()
                .is_some_and(|confirmation| confirmation.confirmed
                    && confirmation.operation_id == id
                    && confirmation.config_digest.as_deref()
                        == Some(outcome.config_digest.as_str())),
            "official reload completion belongs to another operation"
        );
        Ok::<_, anyhow::Error>(data)
    }
    .await;
    match result {
        Ok(data) => {
            if let Err(error) = claim.succeeded(data.clone()) {
                return reload_rejection(
                    &id,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "ERR_RECOVERY_REQUIRED",
                    &error,
                );
            }
            writer.confirm();
            envelope::ok(StatusCode::OK, data)
        }
        Err(error) => {
            let unconfirmed = error.is::<crate::supervisor::ShutdownUnconfirmed>();
            if let Err(persistence) = claim.failed(&error, unconfirmed) {
                return reload_rejection(
                    &id,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "ERR_RECOVERY_REQUIRED",
                    &persistence.context(format!("reload failed: {error:#}")),
                );
            }
            if !unconfirmed {
                writer.confirm();
            }
            claim.record.response(&state, false)
        }
    }
}

#[utoipa::path(
    get,
    path = "/v1/proxy/status",
    params(("X-Request-ID" = Option<String>, Header, description = "Read one reload operation including its original terminal result")),
    responses(
        (status = 200, body = HttpResult<ProxyStatusData>, description = "Proxy release, official confirmation and optional reload operation"),
        (status = 400, body = HttpResult<String>, description = "Invalid request identity or configuration observation"),
        (status = 503, body = HttpResult<ProxyReloadOperation>, description = "Requested reload evidence is unreadable or inconsistent")
    ),
    tag = "Runtime Proxy"
)]
pub(super) async fn status(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Response {
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
    let confirmation = checked_confirmation(&expected).await;
    let requested_id = match super::proxy_reload::requested_id(&headers) {
        Ok(Some(id)) => Some(id),
        Ok(None) => match super::proxy_reload::current_id(&state) {
            Ok(id) => id,
            Err(error) => {
                return envelope::error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "ERR_RECOVERY_REQUIRED",
                    format!("{error:#}"),
                );
            }
        },
        Err(error) => return proxy_error(error),
    };
    let operation = match requested_id {
        Some(id) => match super::proxy_reload::read(&super::proxy_reload::root(&state), &id) {
            Ok(record) => record.map(|record| record.observed_operation(&state)),
            Err(error) => {
                return reload_rejection(
                    &id,
                    StatusCode::SERVICE_UNAVAILABLE,
                    "ERR_RECOVERY_REQUIRED",
                    &error,
                );
            }
        },
        None => None,
    };
    let publication_id = expected.map(|(outcome, _)| outcome.publication_id);
    let data = match state.server.release() {
        Some(release) => ProxyStatusData {
            release_id: Some(release.release_id),
            mode: format!("{:?}", release.pingap.mode).to_ascii_lowercase(),
            configured,
            applied: false,
            publication_id,
            effective_config_path: path.to_string_lossy().to_string(),
            pingap_version: Some(release.pingap.version),
            pingap_commit: Some(release.pingap.commit),
            confirmation,
            operation,
        },
        None => ProxyStatusData {
            release_id: None,
            mode: "idle".to_string(),
            configured,
            applied: false,
            publication_id,
            effective_config_path: path.to_string_lossy().to_string(),
            pingap_version: None,
            pingap_commit: None,
            confirmation,
            operation,
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

async fn checked_confirmation(
    expected: &Option<(
        CompileOutcome,
        crate::proxy::apply_status::ConfirmedPublication,
    )>,
) -> Option<ProxyConfirmation> {
    let mut view = crate::proxy::compiler::official_confirmation()?;
    if !view.confirmed || crate::proxy::compiler::publication_uncertain() {
        view.confirmed = false;
        return Some(view);
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    let verified = match (expected, admin_probe::admin_endpoint()) {
        (Some((outcome, receipt)), Some(endpoint)) => {
            view.proxy_instance_id.as_deref() == Some(receipt.instance_id.as_str())
                && view.config_digest.as_deref() == Some(outcome.config_digest.as_str())
                && crate::proxy::observation::verify_physical(receipt, deadline)
                    .await
                    .is_ok()
                && admin_probe::wait_for_official_publication(
                    endpoint,
                    outcome,
                    receipt.process_id,
                    &receipt.instance_id,
                    deadline.saturating_duration_since(tokio::time::Instant::now()),
                )
                .await
                .is_ok()
                && crate::proxy::observation::verify_physical(receipt, deadline)
                    .await
                    .is_ok()
                && crate::proxy::compiler::official_confirmation().as_ref() == Some(&view)
        }
        (None, Some(_)) | (Some(_), None) | (None, None) => false,
    };
    view.confirmed = verified;
    Some(view)
}

fn reload_rejection(id: &str, status: StatusCode, code: &str, error: &anyhow::Error) -> Response {
    let unknown = code == "ERR_RECOVERY_REQUIRED";
    super::proxy_reload::failure(
        status,
        code,
        &format!("{error:#}"),
        ProxyReloadOperation {
            operation_id: id.into(),
            state: if unknown {
                ProxyReloadState::RecoveryRequired
            } else {
                ProxyReloadState::Failed
            },
            phase: if unknown {
                ProxyPhase::RecoveryRequired
            } else {
                ProxyPhase::Failed
            },
            error_code: Some(code.into()),
            error_message: Some(format!("{error:#}")),
        },
    )
}
