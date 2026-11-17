//! E2B 兼容沙箱门面 handler（feature `kubernetes`）。
//!
//! wire DTO 的字段名与 E2B SDK 期望严格一致（camelCase + ID 大写，如
//! `sandboxID/envdVersion`）；鉴权由 router/sandbox.rs 的 e2b key 中间件
//! 完成（`Extension<SandboxApiKeyRecord>`），数据面走 middleware/sandbox_guard。
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Extension;
use shared_types::sandbox::{SandboxApiKeyRecord, SandboxInstanceRecord, SandboxInstanceState};
use std::sync::Arc;

use crate::router::sandbox::SandboxDeps;
use rcoder_engine::service::sandbox_manager::{CreateOutcome, SandboxApiError, SandboxManager};

fn wire_error(error: SandboxApiError) -> Response {
    let status = match &error {
        SandboxApiError::Unauthorized => StatusCode::UNAUTHORIZED,
        SandboxApiError::NotFound(_) => StatusCode::NOT_FOUND,
        SandboxApiError::Invalid(_) => StatusCode::BAD_REQUEST,
        SandboxApiError::Conflict(_) => StatusCode::CONFLICT,
        SandboxApiError::NotSupported(_) => StatusCode::NOT_IMPLEMENTED,
        SandboxApiError::Unavailable(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let body = serde_json::json!({
        "code": status.as_u16(),
        "message": error.to_string(),
    });
    (status, axum::Json(body)).into_response()
}

fn rfc3339(micros: i64) -> String {
    chrono::DateTime::from_timestamp_micros(micros)
        .map(|time| time.to_rfc3339())
        .unwrap_or_default()
}

#[derive(Debug, serde::Deserialize)]
pub struct CreateSandboxBody {
    #[serde(rename = "templateID", alias = "template", alias = "templateId")]
    pub template: Option<String>,
    pub timeout: Option<i64>,
    #[serde(default)]
    pub envVars: Option<serde_json::Map<String, serde_json::Value>>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, serde::Serialize)]
pub struct SandboxWire {
    pub sandboxID: String,
    pub templateID: String,
    pub clientID: String,
    pub envdVersion: String,
    pub envdAccessToken: String,
    pub trafficAccessToken: String,
    pub domain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub startedAt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endAt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpuCount: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memoryMB: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn wire_from_record(
    record: &SandboxInstanceRecord,
    token: String,
    client_id: String,
    domain: &str,
) -> SandboxWire {
    SandboxWire {
        sandboxID: record.sandbox_id.clone(),
        templateID: record.template_id.clone(),
        clientID: client_id,
        envdVersion: record.envd_version.clone(),
        envdAccessToken: token,
        trafficAccessToken: record.token_hash[..16].to_string(),
        domain: domain.to_string(),
        metadata: record
            .metadata
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok()),
        startedAt: rfc3339(record.created_at_us),
        endAt: record.expires_at_us.map(rfc3339),
        cpuCount: None,
        memoryMB: None,
        state: None,
    }
}

fn detail_wire(
    manager: &SandboxManager,
    record: &SandboxInstanceRecord,
    domain: &str,
) -> SandboxWire {
    let token = manager
        .decrypt_token(record)
        .unwrap_or_else(|_| String::new());
    let mut wire = wire_from_record(record, token, "query".to_string(), domain);
    wire.cpuCount = Some((record.cpu_millicores / 1000).max(1));
    wire.memoryMB = Some(record.memory_mib);
    wire.state = Some(if record.desired_state == SandboxInstanceState::Ready {
        "running".to_string()
    } else {
        record.desired_state.as_str().to_string()
    });
    wire
}

fn domain_of(manager: &SandboxManager) -> String {
    manager.public_domain().to_string()
}

async fn manager_of(deps: &SandboxDeps) -> Result<Arc<SandboxManager>, Response> {
    deps.manager().ok_or_else(|| {
        wire_error(SandboxApiError::Unavailable("e2b sandbox 未启用".into()))
    })
}

pub async fn create_sandbox(
    State(deps): State<SandboxDeps>,
    Extension(key): Extension<SandboxApiKeyRecord>,
    axum::Json(body): axum::Json<CreateSandboxBody>,
) -> Response {
    let manager = match manager_of(&deps).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    let template = match body.template.clone().or_else(|| Some("base".to_string())) {
        Some(template) if !template.is_empty() => template,
        _ => return wire_error(SandboxApiError::Invalid("template 必填".into())),
    };
    let mut env = Vec::new();
    if let Some(map) = &body.envVars {
        for (name, value) in map {
            let value = match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            env.push((name.clone(), value));
        }
    }
    let params = shared_types::sandbox::CreateSandboxParams {
        template_id: template,
        timeout_seconds: body.timeout,
        env,
        metadata: body
            .metadata
            .as_ref()
            .map(|value| value.to_string()),
    };
    match manager.create(&key, &params).await {
        Ok(CreateOutcome { record, token }) => {
            let wire = wire_from_record(&record, token, record.sandbox_id[..8].to_string(), &domain_of(&manager));
            (StatusCode::OK, axum::Json(wire)).into_response()
        }
        Err(error) => wire_error(error),
    }
}

pub async fn get_sandbox(
    State(deps): State<SandboxDeps>,
    Extension(key): Extension<SandboxApiKeyRecord>,
    Path(sandbox_id): Path<String>,
) -> Response {
    let manager = match manager_of(&deps).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    match manager.get(&key, &sandbox_id).await {
        Ok(record) => {
            let wire = detail_wire(&manager, &record, &domain_of(&manager));
            (StatusCode::OK, axum::Json(wire)).into_response()
        }
        Err(error) => wire_error(error),
    }
}

pub async fn list_sandboxes(
    State(deps): State<SandboxDeps>,
    Extension(key): Extension<SandboxApiKeyRecord>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let manager = match manager_of(&deps).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    // M1: 无过滤参数；游标经 x-next-token 请求头（edvabe 同款）。
    let records = match manager.list(&key, None, 100).await {
        Ok(records) => records,
        Err(error) => return wire_error(error),
    };
    let domain = domain_of(&manager);
    let items: Vec<serde_json::Value> = records
        .iter()
        .map(|record| {
            let mut wire = detail_wire(&manager, record, &domain);
            // 列表不泄露 token。
            wire.envdAccessToken = String::new();
            wire.trafficAccessToken = String::new();
            serde_json::to_value(&wire).unwrap_or_default()
        })
        .collect();
    let _ = query;
    (StatusCode::OK, axum::Json(serde_json::Value::Array(items))).into_response()
}

pub async fn kill_sandbox(
    State(deps): State<SandboxDeps>,
    Extension(key): Extension<SandboxApiKeyRecord>,
    Path(sandbox_id): Path<String>,
) -> Response {
    let manager = match manager_of(&deps).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    match manager.kill(&key, &sandbox_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => wire_error(error),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct TimeoutBody {
    pub timeout: i64,
}

pub async fn set_timeout(
    State(deps): State<SandboxDeps>,
    Extension(key): Extension<SandboxApiKeyRecord>,
    Path(sandbox_id): Path<String>,
    axum::Json(body): axum::Json<TimeoutBody>,
) -> Response {
    let manager = match manager_of(&deps).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    match manager.set_timeout(&key, &sandbox_id, body.timeout).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => wire_error(error),
    }
}

pub async fn connect_sandbox(
    State(deps): State<SandboxDeps>,
    Extension(key): Extension<SandboxApiKeyRecord>,
    Path(sandbox_id): Path<String>,
) -> Response {
    let manager = match manager_of(&deps).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    match manager.get(&key, &sandbox_id).await {
        Ok(record) if record.desired_state == SandboxInstanceState::Ready => {
            let wire = detail_wire(&manager, &record, &domain_of(&manager));
            (StatusCode::OK, axum::Json(wire)).into_response()
        }
        Ok(_) => wire_error(SandboxApiError::Invalid("sandbox 非 Ready".into())),
        Err(error) => wire_error(error),
    }
}

pub async fn list_templates(
    State(deps): State<SandboxDeps>,
    Extension(_key): Extension<SandboxApiKeyRecord>,
) -> Response {
    let manager = match manager_of(&deps).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    match manager.list_templates().await {
        Ok(templates) => (StatusCode::OK, axum::Json(serde_json::json!(templates
            .into_iter()
            .map(|summary| serde_json::json!({
                "templateID": summary.template_id,
                "name": summary.display_name.unwrap_or_default(),
                "publicConfigs": [],
            }))
            .collect::<Vec<_>>()))).into_response(),
        Err(error) => wire_error(error),
    }
}

pub async fn not_supported(
    Extension(_key): Extension<SandboxApiKeyRecord>,
) -> Response {
    wire_error(SandboxApiError::NotSupported("e2b sandbox M1 不支持该能力"))
}
