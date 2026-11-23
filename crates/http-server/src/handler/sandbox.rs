//! E2B 兼容沙箱门面 handler（feature `kubernetes`）。
//!
//! wire DTO 的字段名与 E2B SDK 期望严格一致（camelCase + ID 大写，如
//! `sandboxID/envdVersion`）；鉴权由 router/sandbox.rs 的 e2b key 中间件
//! 完成（`Extension<SandboxApiKeyRecord>`），数据面走 middleware/sandbox_guard。
use axum::Extension;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use shared_types::sandbox::{SandboxApiKeyRecord, SandboxInstanceRecord, SandboxInstanceState};
use std::sync::Arc;

use crate::router::sandbox::SandboxDeps;
use rcoder_engine::service::sandbox_manager::{CreateOutcome, SandboxApiError, SandboxManager};

fn wire_error(error: SandboxApiError, locale: &str) -> Response {
    let status = match &error {
        SandboxApiError::Unauthorized => StatusCode::UNAUTHORIZED,
        SandboxApiError::NotFound(_) => StatusCode::NOT_FOUND,
        SandboxApiError::Invalid(_) | SandboxApiError::InvalidCode(_) => StatusCode::BAD_REQUEST,
        SandboxApiError::Conflict(_) | SandboxApiError::ConflictCode(_) => StatusCode::CONFLICT,
        SandboxApiError::NotSupported(_) => StatusCode::NOT_IMPLEMENTED,
        SandboxApiError::Unavailable(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    // 5xx 的内部细节（k8s/PG 错误原文，含 pod 名等）只进日志，不进响应体。
    let message = match &error {
        SandboxApiError::Unavailable(reason) => {
            tracing::warn!(error = %reason, "e2b sandbox 5xx（细节已隐藏）");
            shared_types::error_codes::get_error_message(
                shared_types::error_codes::ERR_SERVICE_UNAVAILABLE,
                locale,
            )
        }
        SandboxApiError::InvalidCode(code) | SandboxApiError::ConflictCode(code) => {
            shared_types::error_codes::get_error_message(code, locale)
        }
        other => other.to_string(),
    };
    let body = serde_json::json!({
        "code": status.as_u16(),
        "message": message,
    });
    (status, axum::Json(body)).into_response()
}

fn rfc3339(micros: i64) -> String {
    chrono::DateTime::from_timestamp_micros(micros)
        .map(|time| time.to_rfc3339())
        .unwrap_or_default()
}

// E2B 兼容线格式：字段名与官方 API 的 camelCase 保持一致，不做 snake_case 改写。
#[allow(non_snake_case)]
#[derive(Debug, serde::Deserialize)]
pub struct CreateSandboxBody {
    #[serde(rename = "templateID", alias = "template", alias = "templateId")]
    pub template: Option<String>,
    pub timeout: Option<i64>,
    #[serde(default)]
    pub envVars: Option<serde_json::Map<String, serde_json::Value>>,
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub volumeMounts: Option<serde_json::Value>,
}

// E2B 兼容线格式：字段名与官方 API 的 camelCase 保持一致，不做 snake_case 改写。
#[allow(non_snake_case)]
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
        // 官方 wire 的 trafficAccessToken 是独立签发的令牌；我们没有等价物，
        // 也不得把存储的 token_hash 前缀当令牌回传（哈希片段泄露）。
        trafficAccessToken: String::new(),
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

// Err 载荷是 axum Response（≥128 字节）：handler 内以 Result<_, Response> 表达
// 短路响应是 axum 惯用形态，装箱会扩散到所有调用点。
#[allow(clippy::result_large_err)]
async fn manager_of(deps: &SandboxDeps, locale: &str) -> Result<Arc<SandboxManager>, Response> {
    deps.manager().ok_or_else(|| {
        wire_error(
            SandboxApiError::Unavailable(shared_types::error_codes::get_error_message(
                shared_types::error_codes::ERR_E2B_NOT_ENABLED,
                locale,
            )),
            locale,
        )
    })
}

/// 解析 `volumeMounts`（两种 wire 形态）→ 挂载清单。Err 载荷是错误码。
///
/// 形态不对一律报错，不再静默丢弃：丢弃会让沙箱创建成功却没挂卷，调用方
/// 拿到一个"看起来正常"的沙箱，直到在里面找不到数据才发觉。
fn parse_volume_mounts(
    mounts: &serde_json::Value,
) -> Result<Vec<shared_types::sandbox::VolumeMountRequest>, &'static str> {
    const INVALID: &str = shared_types::error_codes::ERR_E2B_VOLUME_MOUNTS_INVALID;
    let mut volume_mounts = Vec::new();
    match mounts {
        // 官方 REST wire 格式：数组 [{name: "vol-name", path: "/mount/path"}]
        serde_json::Value::Array(items) => {
            for item in items {
                let Some(obj) = item.as_object() else {
                    return Err(INVALID);
                };
                // name / path 必须存在且是字符串：缺字段或类型错都说明调用方
                // 按错了契约，猜一个默认值只会把错误推后。
                let (Some(name), Some(path)) = (
                    obj.get("name").and_then(|v| v.as_str()),
                    obj.get("path").and_then(|v| v.as_str()),
                ) else {
                    return Err(INVALID);
                };
                if name.is_empty() || path.is_empty() {
                    return Err(INVALID);
                }
                volume_mounts.push(shared_types::sandbox::VolumeMountRequest {
                    volume_name: name.to_string(),
                    mount_path: path.to_string(),
                });
            }
        }
        // SDK 客户端形态：map {mountPath: volumeName 或 {name: volumeName}}
        serde_json::Value::Object(map) => {
            for (mount_path, vol_ref) in map {
                let volume_name = match vol_ref {
                    serde_json::Value::String(name) => name.clone(),
                    serde_json::Value::Object(obj) => obj
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    // 值既不是字符串也不是 {name: ...}：契约不对
                    _ => return Err(INVALID),
                };
                if volume_name.is_empty() || mount_path.is_empty() {
                    return Err(INVALID);
                }
                volume_mounts.push(shared_types::sandbox::VolumeMountRequest {
                    volume_name,
                    mount_path: mount_path.clone(),
                });
            }
        }
        // 字符串/数字/布尔：整体形态就不对。（JSON `null` 走不到这里：
        // `volumeMounts` 是 `Option<Value>`，serde 把 null 反序列化成 None。）
        _ => return Err(INVALID),
    }
    Ok(volume_mounts)
}

pub async fn create_sandbox(
    State(deps): State<SandboxDeps>,
    Extension(key): Extension<SandboxApiKeyRecord>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<CreateSandboxBody>,
) -> Response {
    let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
    let manager = match manager_of(&deps, locale).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    let template = match body.template.clone().or_else(|| Some("base".to_string())) {
        Some(template) if !template.is_empty() => template,
        _ => {
            let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
            return wire_error(
                SandboxApiError::InvalidCode(shared_types::error_codes::ERR_E2B_TEMPLATE_REQUIRED),
                locale,
            );
        }
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
    let mut volume_mounts = Vec::new();
    if let Some(mounts) = &body.volumeMounts {
        match parse_volume_mounts(mounts) {
            Ok(parsed) => volume_mounts = parsed,
            // InvalidCode 载荷是 &'static str：错误码本身就是静态常量，
            // 包装成 String 只会多一次分配（编译期就会被类型系统挡住）。
            Err(code) => return wire_error(SandboxApiError::InvalidCode(code), locale),
        }
    }
    let idempotency_key = headers
        .get("Idempotency-Key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .map(str::to_string);
    let params = shared_types::sandbox::CreateSandboxParams {
        template_id: template,
        timeout_seconds: body.timeout,
        env,
        metadata: body.metadata.as_ref().map(|value| value.to_string()),
        volume_mounts,
        idempotency_key,
    };
    match manager.create(&key, &params).await {
        Ok(CreateOutcome { record, token }) => {
            let wire = wire_from_record(
                &record,
                token,
                record.sandbox_id[..8].to_string(),
                &domain_of(&manager),
            );
            (StatusCode::OK, axum::Json(wire)).into_response()
        }
        Err(error) => {
            let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
            wire_error(error, locale)
        }
    }
}

pub async fn get_sandbox(
    State(deps): State<SandboxDeps>,
    Extension(key): Extension<SandboxApiKeyRecord>,
    headers: axum::http::HeaderMap,
    Path(sandbox_id): Path<String>,
) -> Response {
    let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
    let manager = match manager_of(&deps, locale).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    match manager.get(&key, &sandbox_id).await {
        Ok(record) => {
            let wire = detail_wire(&manager, &record, &domain_of(&manager));
            (StatusCode::OK, axum::Json(wire)).into_response()
        }
        Err(error) => {
            let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
            wire_error(error, locale)
        }
    }
}

pub async fn list_sandboxes(
    State(deps): State<SandboxDeps>,
    Extension(key): Extension<SandboxApiKeyRecord>,
    headers: axum::http::HeaderMap,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> Response {
    let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
    let manager = match manager_of(&deps, locale).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    // M1: 无过滤参数；游标经 x-next-token 请求头（edvabe 同款）。
    let records = match manager.list(&key, None, 100).await {
        Ok(records) => records,
        Err(error) => {
            let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
            return wire_error(error, locale);
        }
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
    let _unused = query;
    (StatusCode::OK, axum::Json(serde_json::Value::Array(items))).into_response()
}

pub async fn kill_sandbox(
    State(deps): State<SandboxDeps>,
    Extension(key): Extension<SandboxApiKeyRecord>,
    headers: axum::http::HeaderMap,
    Path(sandbox_id): Path<String>,
) -> Response {
    let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
    let manager = match manager_of(&deps, locale).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    match manager.kill(&key, &sandbox_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
            wire_error(error, locale)
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct TimeoutBody {
    pub timeout: i64,
}

pub async fn set_timeout(
    State(deps): State<SandboxDeps>,
    Extension(key): Extension<SandboxApiKeyRecord>,
    headers: axum::http::HeaderMap,
    Path(sandbox_id): Path<String>,
    axum::Json(body): axum::Json<TimeoutBody>,
) -> Response {
    let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
    let manager = match manager_of(&deps, locale).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    match manager.set_timeout(&key, &sandbox_id, body.timeout).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
            wire_error(error, locale)
        }
    }
}

pub async fn connect_sandbox(
    State(deps): State<SandboxDeps>,
    Extension(key): Extension<SandboxApiKeyRecord>,
    headers: axum::http::HeaderMap,
    Path(sandbox_id): Path<String>,
) -> Response {
    let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
    let manager = match manager_of(&deps, locale).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    match manager.get(&key, &sandbox_id).await {
        Ok(record) if record.desired_state == SandboxInstanceState::Ready => {
            let wire = detail_wire(&manager, &record, &domain_of(&manager));
            (StatusCode::OK, axum::Json(wire)).into_response()
        }
        Ok(_) => {
            let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
            wire_error(
                SandboxApiError::InvalidCode(shared_types::error_codes::ERR_E2B_NOT_READY),
                locale,
            )
        }
        Err(error) => {
            let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
            wire_error(error, locale)
        }
    }
}

pub async fn list_templates(
    State(deps): State<SandboxDeps>,
    Extension(_key): Extension<SandboxApiKeyRecord>,
    headers: axum::http::HeaderMap,
) -> Response {
    let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
    let manager = match manager_of(&deps, locale).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    match manager.list_templates().await {
        Ok(templates) => (
            StatusCode::OK,
            axum::Json(serde_json::json!(
                templates
                    .into_iter()
                    .map(|summary| serde_json::json!({
                        "templateID": summary.template_id,
                        "name": summary.display_name.unwrap_or_default(),
                        "publicConfigs": [],
                    }))
                    .collect::<Vec<_>>()
            )),
        )
            .into_response(),
        Err(error) => {
            let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
            wire_error(error, locale)
        }
    }
}

pub async fn not_supported(
    Extension(_key): Extension<SandboxApiKeyRecord>,
    headers: axum::http::HeaderMap,
) -> Response {
    let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
    wire_error(
        SandboxApiError::NotSupported("e2b sandbox M1 does not support this capability"),
        locale,
    )
}

// ---------- Volume handlers（M5） ----------

// E2B 兼容线格式：字段名与官方 API 的 camelCase 保持一致，不做 snake_case 改写。
#[allow(non_snake_case)]
#[derive(Debug, serde::Deserialize)]
pub struct CreateVolumeBody {
    pub name: String,
    #[serde(default = "default_volume_size")]
    pub size: i32,
}

fn default_volume_size() -> i32 {
    1
}

// E2B 兼容线格式：字段名与官方 API 的 camelCase 保持一致，不做 snake_case 改写。
#[allow(non_snake_case)]
#[derive(Debug, serde::Serialize)]
pub struct VolumeWire {
    pub volumeID: String,
    pub name: String,
    pub sizeGiB: i32,
    pub createdAt: String,
}

pub async fn create_volume(
    State(deps): State<SandboxDeps>,
    Extension(key): Extension<SandboxApiKeyRecord>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<CreateVolumeBody>,
) -> Response {
    let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
    let manager = match manager_of(&deps, locale).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    match manager.create_volume(&key, &body.name, body.size).await {
        Ok(record) => {
            let wire = VolumeWire {
                volumeID: record.volume_id,
                name: record.name,
                sizeGiB: record.size_gib,
                createdAt: rfc3339(record.created_at_us),
            };
            (StatusCode::CREATED, axum::Json(wire)).into_response()
        }
        Err(error) => {
            let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
            wire_error(error, locale)
        }
    }
}

pub async fn list_volumes(
    State(deps): State<SandboxDeps>,
    Extension(key): Extension<SandboxApiKeyRecord>,
    headers: axum::http::HeaderMap,
) -> Response {
    let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
    let manager = match manager_of(&deps, locale).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    match manager.list_volumes(&key).await {
        Ok(records) => {
            let items: Vec<VolumeWire> = records
                .into_iter()
                .map(|r| VolumeWire {
                    volumeID: r.volume_id,
                    name: r.name,
                    sizeGiB: r.size_gib,
                    createdAt: rfc3339(r.created_at_us),
                })
                .collect();
            (StatusCode::OK, axum::Json(items)).into_response()
        }
        Err(error) => {
            let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
            wire_error(error, locale)
        }
    }
}

pub async fn delete_volume(
    State(deps): State<SandboxDeps>,
    Extension(key): Extension<SandboxApiKeyRecord>,
    Path(volume_id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
    let manager = match manager_of(&deps, locale).await {
        Ok(manager) => manager,
        Err(response) => return response,
    };
    match manager.delete_volume(&key, &volume_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            let locale = crate::handler::utils::locale::get_locale_from_headers(&headers);
            wire_error(error, locale)
        }
    }
}

#[cfg(test)]
mod volume_mounts_tests {
    use super::parse_volume_mounts;
    use shared_types::error_codes::ERR_E2B_VOLUME_MOUNTS_INVALID;

    fn json(value: serde_json::Value) -> serde_json::Value {
        value
    }

    #[test]
    fn array_wire_form_parses_name_and_path() {
        let mounts = parse_volume_mounts(&json(serde_json::json!([
            {"name": "vol-a", "path": "/data"},
            {"name": "vol-b", "path": "/mnt/b"},
        ])))
        .unwrap();
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0].volume_name, "vol-a");
        assert_eq!(mounts[0].mount_path, "/data");
    }

    #[test]
    fn sdk_object_form_accepts_string_and_object_refs() {
        let mounts = parse_volume_mounts(&json(serde_json::json!({
            "/data": "vol-a",
            "/mnt/b": {"name": "vol-b"},
        })))
        .unwrap();
        assert_eq!(mounts.len(), 2);
        assert!(
            mounts
                .iter()
                .any(|m| m.mount_path == "/data" && m.volume_name == "vol-a")
        );
        assert!(
            mounts
                .iter()
                .any(|m| m.mount_path == "/mnt/b" && m.volume_name == "vol-b")
        );
    }

    #[test]
    fn absent_mounts_key_is_not_an_error() {
        // None 由调用方分流（不进本函数）；空数组 = 合法地不挂卷
        let mounts = parse_volume_mounts(&json(serde_json::json!([]))).unwrap();
        assert!(mounts.is_empty());
    }

    #[test]
    fn array_item_that_is_not_an_object_is_rejected() {
        let error = parse_volume_mounts(&json(serde_json::json!(["vol-a"]))).unwrap_err();
        assert_eq!(error, ERR_E2B_VOLUME_MOUNTS_INVALID);
    }

    #[test]
    fn array_item_missing_or_non_string_field_is_rejected() {
        // 缺 name
        assert_eq!(
            parse_volume_mounts(&json(serde_json::json!([{"path": "/data"}]))).unwrap_err(),
            ERR_E2B_VOLUME_MOUNTS_INVALID
        );
        // 缺 path
        assert_eq!(
            parse_volume_mounts(&json(serde_json::json!([{"name": "vol-a"}]))).unwrap_err(),
            ERR_E2B_VOLUME_MOUNTS_INVALID
        );
        // path 是数字（类型错，不是空串）
        assert_eq!(
            parse_volume_mounts(&json(serde_json::json!([{"name": "vol-a", "path": 7}])))
                .unwrap_err(),
            ERR_E2B_VOLUME_MOUNTS_INVALID
        );
    }

    #[test]
    fn empty_name_or_path_is_rejected() {
        assert_eq!(
            parse_volume_mounts(&json(serde_json::json!([{"name": "", "path": "/data"}])))
                .unwrap_err(),
            ERR_E2B_VOLUME_MOUNTS_INVALID
        );
        assert_eq!(
            parse_volume_mounts(&json(serde_json::json!([{"name": "vol-a", "path": ""}])))
                .unwrap_err(),
            ERR_E2B_VOLUME_MOUNTS_INVALID
        );
    }

    #[test]
    fn object_form_value_of_wrong_type_is_rejected() {
        // 值既不是字符串也不是 {name: ...}
        let error = parse_volume_mounts(&json(serde_json::json!({"/data": 42}))).unwrap_err();
        assert_eq!(error, ERR_E2B_VOLUME_MOUNTS_INVALID);
        // {name: 7} 同样拿不到字符串
        let error =
            parse_volume_mounts(&json(serde_json::json!({"/data": {"name": 7}}))).unwrap_err();
        assert_eq!(error, ERR_E2B_VOLUME_MOUNTS_INVALID);
    }

    #[test]
    fn scalar_top_level_is_rejected() {
        for value in [
            serde_json::json!("vol-a"),
            serde_json::json!(7),
            serde_json::json!(true),
            serde_json::json!(null),
        ] {
            assert_eq!(
                parse_volume_mounts(&json(value)).unwrap_err(),
                ERR_E2B_VOLUME_MOUNTS_INVALID,
                "标量形态必须报错而不是被静默忽略"
            );
        }
    }
}
