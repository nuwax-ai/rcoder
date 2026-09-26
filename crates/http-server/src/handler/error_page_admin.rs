//! UserApp 错误页管理接口（`/api/v1/admin/userapp/error-page`）。
//!
//! 一份部署级页面、三个管理接口（PUT/GET/DELETE）：上传本地 HTML、查询
//! 保存/加载状态、恢复内置默认。沿用全局 API Key middleware（挂载于
//! create_router 保护域内）；独立正文上限 512 KiB（不继承项目上传 1 GiB）。
//!
//! 语义（proxy-error-page.md §8）：
//! - PUT 成功 = 权威存储已保存；本副本是否已加载分字段透出，不冒充全副本生效；
//! - DELETE 幂等移除覆盖恢复内置；K8s 只删页面 key 保留同对象其他内容；
//! - 未配置存储后端：PUT/DELETE 503 `ERROR_PAGE_STORAGE_NOT_CONFIGURED`
//!   （GET 仍返回 builtin/未配置状态）；写只读目录/存储故障不假成功；
//! - 类型不符 415、过大 413、空内容/编码/占位符错误 422、并发冲突 409。

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::put;
use axum::{Json, Router};
use rcoder_engine::app_state::AppState;
use rcoder_engine::userapp_error_page::{
    ErrorPageAdminStatus, ErrorPageStoreError, ErrorPageWriteResult, MAX_PAGE_BYTES,
};
use shared_types::HttpResult;

/// PUT 正文上限（axum 层 413；服务层再按实际字节数校验）。
const PUT_BODY_LIMIT: usize = MAX_PAGE_BYTES + 1;

/// 未装配页面服务时的进程实例标识（GET 状态透出用）。
fn fallback_instance_id() -> &'static str {
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| uuid::Uuid::new_v4().simple().to_string())
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/v1/admin/userapp/error-page",
            put(put_error_page)
                .get(get_error_page)
                .delete(delete_error_page),
        )
        .layer(DefaultBodyLimit::max(PUT_BODY_LIMIT))
}

fn no_store<T: serde::Serialize>(status: StatusCode, payload: HttpResult<T>) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], Json(payload)).into_response()
}

fn error_response(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    let message = message.into();
    no_store(status, HttpResult::<()>::error(code, &message))
}

fn store_error_response(error: ErrorPageStoreError) -> Response {
    let status = match error.code {
        "ERROR_PAGE_STORAGE_NOT_CONFIGURED" => StatusCode::SERVICE_UNAVAILABLE,
        "ERROR_PAGE_INVALID" => StatusCode::UNPROCESSABLE_ENTITY,
        "ERROR_PAGE_STORE_CONFLICT" => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_response(status, error.code, error.message)
}

#[utoipa::path(
    put,
    path = "/api/v1/admin/userapp/error-page",
    summary = "上传部署级 UserApp 故障页（≤512KiB UTF-8 HTML，四个转义占位符）",
    request_body(content = String, content_type = "text/html"),
    description = r#"
上传一份本地 HTML 作为部署级 UserApp 故障页（权威存储原子替换 + 本副本热加载）。

- 正文 `text/html`（charset 缺省按 UTF-8 处理）；单文件 ≤ 512 KiB、非空、UTF-8；
- 仅支持 `{{RCODER_TITLE}}` / `{{RCODER_MESSAGE}}` / `{{RCODER_DIAGNOSTIC_ID}}` /
  `{{RCODER_STATUS}}` 四个转义文本占位符，未知 `{{RCODER_*}}` 拒绝（422）；
  无占位符的完整静态页合法；
- 成功 = 权威已保存；`loaded_by_this_replica` 才表示本副本已加载（K8s 多副本
  经 ConfigMap 投射最终收敛，不承诺立即全副本切换）；
- 415 类型不符 / 413 过大 / 422 校验失败 / 409 并发冲突 /
  503 存储未配置；失败保留旧页。
"#,
    responses(
        (status = 200, description = "权威保存成功（返回保存与本副本加载摘要）", body = HttpResult<ErrorPageWriteResult>),
        (status = 413, description = "正文超过 512 KiB", body = HttpResult<String>),
        (status = 415, description = "Content-Type 不是 text/html", body = HttpResult<String>),
        (status = 422, description = "空内容/非 UTF-8/未知占位符", body = HttpResult<String>),
        (status = 503, description = "存储后端未配置", body = HttpResult<String>)
    ),
    tag = "Userapp · 访问入口"
)]
async fn put_error_page(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let Some(service) = state.userapp_error_page.as_ref() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "ERROR_PAGE_STORAGE_NOT_CONFIGURED",
            "error page storage backend is not configured for this deployment",
        );
    };
    // Content-Type：声明了就必须是 text/html（允许 charset 参数）；未声明按
    // HTML 接收（管理面/Make 固定发送，缺失不构成伪造风险）。
    if let Some(content_type) = headers.get(header::CONTENT_TYPE)
        && let Ok(content_type) = content_type.to_str()
        && !content_type
            .split(';')
            .next()
            .map(str::trim_ascii)
            .is_some_and(|media| media.eq_ignore_ascii_case("text/html"))
    {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "ERROR_PAGE_CONTENT_TYPE",
            format!("Content-Type must be text/html (got: {content_type})"),
        );
    }
    if body.len() > MAX_PAGE_BYTES {
        return error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "ERROR_PAGE_TOO_LARGE",
            format!(
                "error page exceeds {MAX_PAGE_BYTES} bytes (got {})",
                body.len()
            ),
        );
    }
    match service.save(&body).await {
        Ok(result) => no_store(StatusCode::OK, HttpResult::success(result)),
        Err(error) => store_error_response(error),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/userapp/error-page",
    summary = "查询故障页权威/本副本加载状态与 in_sync",
    description = r#"
查询错误页管理状态：权威保存摘要、本副本加载摘要/来源/时间/错误码、实例
标识与 `in_sync`（本副本缓存与权威是否一致）。权威无覆盖且本副本用内置页
时 `in_sync=true`；`stored_sha256` 与 `loaded_sha256` 短暂不同属正常
（K8s ConfigMap 投射/缓存传播延迟）。GET 触发一次按需缓存检查（≤5s 节流）。
"#,
    responses(
        (status = 200, description = "管理状态", body = HttpResult<ErrorPageAdminStatus>)
    ),
    tag = "Userapp · 访问入口"
)]
async fn get_error_page(State(state): State<Arc<AppState>>) -> Response {
    let Some(service) = state.userapp_error_page.as_ref() else {
        // 未配置存储：仍返回可读状态（内置页 + 未配置后端），不误报错误。
        let status = ErrorPageAdminStatus {
            instance_id: fallback_instance_id().to_string(),
            stored: rcoder_engine::userapp_error_page::ErrorPageStoredSummary {
                stored_sha256: None,
            },
            loaded_sha256: None,
            loaded_source: Some("builtin".into()),
            loaded_at: None,
            last_checked_at: None,
            last_load_error_code: Some("ERROR_PAGE_STORAGE_NOT_CONFIGURED".into()),
            in_sync: true,
            backend: "not-configured".into(),
        };
        return no_store(StatusCode::OK, HttpResult::success(status));
    };
    match service.admin_status().await {
        Ok(status) => no_store(StatusCode::OK, HttpResult::success(status)),
        Err(error) => store_error_response(error),
    }
}

#[utoipa::path(
    delete,
    path = "/api/v1/admin/userapp/error-page",
    summary = "恢复内置默认故障页（幂等移除权威覆盖）",
    description = r#"
恢复内置默认页：幂等移除权威覆盖（K8s 只删页面 key，保留同一 ConfigMap
其他内容；不存在时同样成功）。多副本按相同传播规则最终生效。
"#,
    responses(
        (status = 200, description = "已恢复内置页（幂等）", body = HttpResult<ErrorPageWriteResult>),
        (status = 503, description = "存储后端未配置", body = HttpResult<String>)
    ),
    tag = "Userapp · 访问入口"
)]
async fn delete_error_page(State(state): State<Arc<AppState>>) -> Response {
    let Some(service) = state.userapp_error_page.as_ref() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "ERROR_PAGE_STORAGE_NOT_CONFIGURED",
            "error page storage backend is not configured for this deployment",
        );
    };
    match service.delete().await {
        Ok(result) => no_store(StatusCode::OK, HttpResult::success(result)),
        Err(error) => store_error_response(error),
    }
}
