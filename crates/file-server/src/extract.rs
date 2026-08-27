//! HTTP 提取器适配层：把 Axum 原生 rejection 统一映射为 file-server `AppError`。
//!
//! 另含 userApp 分流标记（`USERAPP_FLAG`）：`X-Service-Type: userapp` 请求经
//! 反向代理/rcoder 拦截层透传到容器内，由 [`scope_userapp_flag`] 中间件读 header
//! 注入 task-local，computer 域 workspace 定位（`ws_path` 等）据此切换到
//! userApp 开发卷——HTTP 层标记，与 ServiceType 枚举（容器编排层）互不相干。

use axum::extract::multipart::MultipartRejection;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Multipart, Path, Query, Request};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};

use crate::error::AppError;

// 分流契约常量与 rcoder 转发层共用 shared_types 单一事实源。
pub use shared_types::{SERVICE_TYPE_HEADER, SERVICE_TYPE_USERAPP};

// ── 请求级 userApp 分流标记 (task_local, 由请求中间件 scope 注入) ────────────────
tokio::task_local! {
    static USERAPP_FLAG: bool;
}

/// 当前请求是否为 userApp 场景（`X-Service-Type: userapp`；task_local 未设置时 false）。
pub fn is_userapp_request() -> bool {
    USERAPP_FLAG.try_with(|f| *f).unwrap_or(false)
}

/// 中间件：读 `X-Service-Type` header → task-local scope 注入（全部 handler 可读）。
pub async fn scope_userapp_flag(req: Request, next: axum::middleware::Next) -> Response {
    let is_userapp = req
        .headers()
        .get(SERVICE_TYPE_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(shared_types::is_userapp_service_type_value);
    USERAPP_FLAG.scope(is_userapp, next.run(req)).await
}

pub struct AppJson<T>(pub T);

impl<S, T> FromRequest<S> for AppJson<T>
where
    S: Send + Sync,
    axum::Json<T>: FromRequest<S, Rejection = JsonRejection>,
{
    type Rejection = AppError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        axum::Json::<T>::from_request(request, state)
            .await
            .map(|axum::Json(value)| Self(value))
            .map_err(|error| AppError::validation(format!("invalid JSON request: {error}")))
    }
}

impl<T> IntoResponse for AppJson<T>
where
    axum::Json<T>: IntoResponse,
{
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

pub struct AppQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for AppQuery<T>
where
    S: Send + Sync,
    Query<T>: FromRequestParts<S, Rejection = QueryRejection>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(|error| AppError::validation(format!("invalid query parameters: {error}")))
    }
}

pub struct AppPath<T>(pub T);

impl<S, T> FromRequestParts<S> for AppPath<T>
where
    S: Send + Sync,
    Path<T>: FromRequestParts<S, Rejection = PathRejection>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| Self(value))
            .map_err(|error| AppError::validation(format!("invalid path parameters: {error}")))
    }
}

pub struct AppMultipart(Multipart);

impl<S> FromRequest<S> for AppMultipart
where
    S: Send + Sync,
    Multipart: FromRequest<S, Rejection = MultipartRejection>,
{
    type Rejection = AppError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        Multipart::from_request(request, state)
            .await
            .map(Self)
            .map_err(|error| AppError::validation(format!("invalid multipart request: {error}")))
    }
}

impl std::ops::Deref for AppMultipart {
    type Target = Multipart;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for AppMultipart {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

// ── ID 字段反序列化 helper ────────────────────────────────────────────────────────
// 对齐 TS 原版 `String(id)` 弱类型容错: Java 后端 / 前端可能传 `agentId: 17`(DB bigint
// 整数) 或 `"17"`(字符串), 这里统一接受为 String, 避免 serde "invalid type: integer" 报错。
// 用法: #[serde(deserialize_with = "crate::extract::deserialize_id_string")]

/// 兼容整数 + 字符串的 deserializer, 用于必填 ID 字段 (project_id / agent_id / user_id 等)。
pub fn deserialize_id_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    use serde::de::Error;
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        _ => Err(Error::custom("expected string or number")),
    }
}

/// `Option<String>` 版本, 用于可选 ID 字段 (tenant_id / space_id 等, 缺省为 None)。
pub fn deserialize_optional_id_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    use serde::de::Error;
    match Option::<serde_json::Value>::deserialize(deserializer)? {
        None => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s)),
        Some(serde_json::Value::Number(n)) => Ok(Some(n.to_string())),
        Some(_) => Err(Error::custom("expected string or number")),
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use serde::Deserialize;
    use tower::ServiceExt;

    use super::{AppJson, is_userapp_request};

    /// task-local 分流标记：中间件 scope 注入后 handler 内可读，
    /// 未设置（中间件外/普通请求）恒 false。
    #[tokio::test]
    async fn userapp_flag_scoped_by_middleware() {
        let app = Router::new().route(
            "/probe",
            post(|| async { format!("{}", is_userapp_request()) }),
        );
        let app = app.layer(axum::middleware::from_fn(super::scope_userapp_flag));

        // 带 X-Service-Type: userapp → handler 内 true
        let resp = app
            .clone()
            .oneshot(
                Request::post("/probe")
                    .header(super::SERVICE_TYPE_HEADER, super::SERVICE_TYPE_USERAPP)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(to_bytes(resp.into_body(), 1024).await.unwrap(), "true");

        // 大小写不敏感变体（单一事实源 shared_types::is_userapp_service_type_value）
        for variant in ["UserApp", "USERAPP"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::post("/probe")
                        .header(super::SERVICE_TYPE_HEADER, variant)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                to_bytes(resp.into_body(), 1024).await.unwrap(),
                "true",
                "{variant}"
            );
        }

        // 无 header → false
        let resp = app
            .oneshot(Request::post("/probe").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(to_bytes(resp.into_body(), 1024).await.unwrap(), "false");
    }

    #[derive(Deserialize)]
    struct Input {
        value: String,
    }

    async fn handler(AppJson(input): AppJson<Input>) -> AppJson<serde_json::Value> {
        AppJson(serde_json::json!({ "success": true, "value": input.value }))
    }

    #[tokio::test]
    async fn malformed_json_uses_unified_error_response() {
        let app = Router::new().route("/", post(handler));
        let request = Request::builder()
            .method("POST")
            .uri("/")
            .header("content-type", "application/json")
            .body(Body::from("{invalid"))
            .expect("valid request fixture");

        let response = app.oneshot(request).await.expect("router response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("error response body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("JSON error response");
        assert_eq!(json["success"], false);
        assert_eq!(json["error"]["type"], "VALIDATION_ERROR");
    }

    /// 回归保护: ID 字段必须同时接受整数 (Java 后端传 DB bigint, 如 agentId:17)
    /// 与字符串 (如 "17"), 对齐 TS 原版 String(id) 弱类型容错。
    #[test]
    fn id_string_field_accepts_integer_and_string() {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Body {
            #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
            id: String,
            #[serde(
                default,
                deserialize_with = "crate::extract::deserialize_optional_id_string"
            )]
            tenant_id: Option<String>,
        }

        // 整数 (复现 agentId:17 报错场景)
        let body: Body = serde_json::from_str(r#"{"id":17,"tenantId":5}"#).unwrap();
        assert_eq!(body.id, "17");
        assert_eq!(body.tenant_id.as_deref(), Some("5"));

        // 字符串 (原有行为不回归)
        let body: Body = serde_json::from_str(r#"{"id":"x","tenantId":"y"}"#).unwrap();
        assert_eq!(body.id, "x");
        assert_eq!(body.tenant_id.as_deref(), Some("y"));

        // 可选字段缺失 → None
        let body: Body = serde_json::from_str(r#"{"id":1}"#).unwrap();
        assert_eq!(body.id, "1");
        assert!(body.tenant_id.is_none());

        // 可选字段显式 null → None
        let body: Body = serde_json::from_str(r#"{"id":1,"tenantId":null}"#).unwrap();
        assert_eq!(body.id, "1");
        assert!(body.tenant_id.is_none());
    }
}
