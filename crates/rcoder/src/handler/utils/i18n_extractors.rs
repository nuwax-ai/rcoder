//! Locale-aware request extractors.
//!
//! These wrappers map Axum extractor rejections into unified `AppError` so
//! HTTP error responses are consistently localized through `AppError::into_response`.
//!
//! `I18nJson` / `require_field` / `map_json_rejection` 当前未被路由直接使用，
//! 由其它 i18n 入口替代，但保留以备复用。

#![allow(dead_code)]

use std::collections::HashMap;

use axum::{
    extract::{
        FromRequest, FromRequestParts, Json, Path, Query, Request,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::request::Parts,
};
use axum::http::Uri;
use serde::de::DeserializeOwned;

use crate::AppError;

/// Locale-aware JSON extractor.
pub struct I18nJson<T>(pub T);

impl<S, T> FromRequest<S> for I18nJson<T>
where
    S: Send + Sync,
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(map_json_rejection(rejection)),
        }
    }
}

/// Locale-aware query extractor.
pub struct I18nQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for I18nQuery<T>
where
    S: Send + Sync,
    Query<T>: FromRequestParts<S, Rejection = QueryRejection>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(map_query_rejection(rejection)),
        }
    }
}

/// Locale-aware path extractor.
pub struct I18nPath<T>(pub T);

impl<S, T> FromRequestParts<S> for I18nPath<T>
where
    S: Send + Sync,
    Path<T>: FromRequestParts<S, Rejection = PathRejection>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Path::<T>::from_request_parts(parts, state).await {
            Ok(Path(value)) => Ok(Self(value)),
            Err(rejection) => Err(map_path_rejection(rejection)),
        }
    }
}

/// 🆕 JSON 或 Query 参数提取器
///
/// 支持两种输入方式：
/// 1. JSON body: `{"project_id": "xxx"}`
/// 2. Query params: `?project_id=xxx`
///
/// 如果两者同时存在，优先使用 JSON body。
///
/// 适用于需要同时兼容 GET（query）和 POST（body）两种调用方式的接口。
pub struct I18nJsonOrQuery<T>(pub T);

impl<S, T> FromRequest<S> for I18nJsonOrQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned + serde::Serialize + Send,
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        // 1. 使用 http::Uri 提取 query string
        let uri: Uri = req.uri().clone();
        let query_string = uri.query().unwrap_or("");

        // 2. 解析 query string 为 serde_json::Value
        let query_value: serde_json::Value = if query_string.is_empty() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            // 使用 serde_urlencoded 解析为 HashMap，然后转为 Value
            match serde_urlencoded::from_str::<HashMap<String, serde_json::Value>>(query_string) {
                Ok(map) => serde_json::Value::Object(map.into_iter().collect()),
                Err(_) => {
                    return Err(AppError::with_i18n_key(
                        shared_types::error_codes::ERR_INVALID_PARAMS,
                        "error.invalid_params",
                    ));
                }
            }
        };

        // 3. 尝试解析 JSON body
        match Json::<T>::from_request(req, state).await {
            Ok(Json(json_value)) => {
                // 4. JSON 存在，合并两者（JSON 优先）
                let json_value_as_value = serde_json::to_value(&json_value)
                    .unwrap_or(serde_json::Value::Null);
                let merged = deep_merge(query_value, json_value_as_value);
                // 反序列化为 T（只需要 DeserializeOwned）
                let result = serde_json::from_value(merged)
                    .map_err(|_| AppError::with_i18n_key(
                        shared_types::error_codes::ERR_INVALID_PARAMS,
                        "error.invalid_params",
                    ))?;
                Ok(Self(result))
            }
            Err(_) => {
                // 5. JSON 解析失败，检查 query string 是否为空
                if query_string.is_empty() {
                    // 两者都为空，返回参数错误
                    return Err(AppError::with_i18n_key(
                        shared_types::error_codes::ERR_INVALID_PARAMS,
                        "error.invalid_params",
                    ));
                }
                // 使用 query string 反序列化
                let result = serde_json::from_value(query_value)
                    .map_err(|_| AppError::with_i18n_key(
                        shared_types::error_codes::ERR_INVALID_PARAMS,
                        "error.invalid_params",
                    ))?;
                Ok(Self(result))
            }
        }
    }
}

impl<T> I18nJsonOrQuery<T>
where
    T: garde::Validate,
    T::Context: Default,
{
    /// 校验并转换为 AppError
    ///
    /// 使用方法：
    /// ```ignore
    /// let I18nJsonOrQuery(request) = I18nJsonOrQuery(request).validate_into_app_error()?;
    /// ```
    pub fn validate_into_app_error(self) -> Result<Self, AppError> {
        self.0.validate().map_err(shared_types::garde_err_to_app_error)?;
        Ok(self)
    }
}

/// 深度合并两个 JSON 对象
///
/// base: 基础值（query string）
/// override: 覆盖值（JSON body）
///
/// 规则：override 中的非 null 值会覆盖 base 中的对应值
fn deep_merge(base: serde_json::Value, override_: serde_json::Value) -> serde_json::Value {
    match (base, override_) {
        (serde_json::Value::Object(mut base_map), serde_json::Value::Object(override_map)) => {
            for (key, override_value) in override_map {
                let base_value = base_map.remove(&key).unwrap_or(serde_json::Value::Null);
                let merged_value = if override_value.is_null() {
                    // override 值为 null，使用 base 的值
                    base_value
                } else if override_value.is_object() && base_value.is_object() {
                    // 两者都是对象，递归合并
                    deep_merge(base_value, override_value)
                } else {
                    // override 有值（非 null 且不是对象），直接使用 override
                    override_value
                };
                base_map.insert(key, merged_value);
            }
            serde_json::Value::Object(base_map)
        }
        // 如果 override 不是对象，直接使用 override
        (_, override_) => override_,
    }
}

/// 从 Option<String> 中安全提取非空字符串
pub fn require_field<'a>(value: &'a Option<String>, field_name: &str) -> Result<&'a str, AppError> {
    value
        .as_ref()
        .filter(|s| !s.is_empty())
        .map(|s| s.as_str())
        .ok_or_else(|| AppError::with_i18n_key(
            shared_types::error_codes::ERR_VALIDATION,
            &format!("{} is required", field_name),
        ))
}

fn map_json_rejection(rejection: JsonRejection) -> AppError {
    use shared_types::error_codes::{ERR_INVALID_PARAMS, ERR_VALIDATION};

    match rejection {
        JsonRejection::JsonDataError(_) => {
            AppError::with_i18n_key(ERR_VALIDATION, "error.validation")
        }
        JsonRejection::JsonSyntaxError(_)
        | JsonRejection::MissingJsonContentType(_)
        | JsonRejection::BytesRejection(_) => {
            AppError::with_i18n_key(ERR_INVALID_PARAMS, "error.invalid_params")
        }
        _ => AppError::with_i18n_key(ERR_INVALID_PARAMS, "error.invalid_params"),
    }
}

fn map_query_rejection(_rejection: QueryRejection) -> AppError {
    AppError::with_i18n_key(
        shared_types::error_codes::ERR_INVALID_PARAMS,
        "error.invalid_params",
    )
}

fn map_path_rejection(_rejection: PathRejection) -> AppError {
    AppError::with_i18n_key(
        shared_types::error_codes::ERR_INVALID_PARAMS,
        "error.invalid_params",
    )
}

#[cfg(test)]
mod tests {
    use axum::{
        body::to_bytes,
        Router,
        body::Body,
        http::{Request, StatusCode},
        response::IntoResponse,
        routing::{get, post},
    };
    use serde::{Deserialize, Serialize};
    use tower::ServiceExt;

    use super::{I18nJson, I18nJsonOrQuery, I18nPath, I18nQuery};

    #[derive(Deserialize)]
    struct Payload {
        value: i32,
    }

    #[derive(Deserialize)]
    struct QueryParams {
        limit: u32,
    }

    /// 模拟 StopAgentQuery 结构体
    /// 注意：project_id 使用 Option 以测试缺失字段的情况
    #[derive(Deserialize, Serialize, Debug)]
    struct StopAgentQuery {
        #[serde(default)]
        project_id: Option<String>,
        #[serde(default)]
        pod_id: Option<String>,
        #[serde(default)]
        tenant_id: Option<String>,
        #[serde(default)]
        space_id: Option<String>,
        #[serde(default)]
        isolation_type: Option<String>,
    }

    async fn json_handler(I18nJson(payload): I18nJson<Payload>) -> impl IntoResponse {
        payload.value.to_string()
    }

    async fn query_handler(I18nQuery(params): I18nQuery<QueryParams>) -> impl IntoResponse {
        params.limit.to_string()
    }

    async fn path_handler(I18nPath(id): I18nPath<u32>) -> impl IntoResponse {
        id.to_string()
    }

    /// I18nJsonOrQuery 测试处理器
    async fn stop_handler(I18nJsonOrQuery(params): I18nJsonOrQuery<StopAgentQuery>) -> impl IntoResponse {
        format!("project_id={}", params.project_id.unwrap_or_default())
    }

    // ==================== I18nJsonOrQuery 测试 ====================

    #[tokio::test]
    async fn test_i18n_json_or_query_with_only_json_body() {
        let app = Router::new().route("/stop", post(stop_handler));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/stop")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"project_id": "test_project"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"project_id=test_project");
    }

    #[tokio::test]
    async fn test_i18n_json_or_query_with_only_query_string() {
        let app = Router::new().route("/stop", post(stop_handler));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/stop?project_id=test_project")
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"project_id=test_project");
    }

    #[tokio::test]
    async fn test_i18n_json_or_query_with_json_body_and_query_string_prefers_json() {
        let app = Router::new().route("/stop", post(stop_handler));

        // 发送 JSON body 和 query string，JSON 应该优先
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/stop?project_id=from_query")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"project_id": "from_json"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"project_id=from_json");
    }

    #[tokio::test]
    async fn test_i18n_json_or_query_with_query_string_and_all_fields() {
        let app = Router::new().route("/stop", post(stop_handler));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/stop?project_id=proj123&pod_id=pod456&tenant_id=tenant789")
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"project_id=proj123");
    }

    #[tokio::test]
    async fn test_i18n_json_or_query_empty_body_and_empty_query_returns_error() {
        let app = Router::new().route("/stop", post(stop_handler));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/stop")
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // 应该返回 BAD_REQUEST 因为既没有 JSON body 也没有有效的 query string
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_i18n_json_or_query_invalid_json_returns_error() {
        let app = Router::new().route("/stop", post(stop_handler));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/stop")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"project_id":}"#)) // invalid JSON
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_i18n_json_or_query_missing_project_id_in_json() {
        let app = Router::new().route("/stop", post(stop_handler));

        // JSON body 缺少 project_id 字段，但 serde 会成功解析（project_id 是 String 类型会默认null）
        // 不过 StopAgentQuery 的 project_id 是必填字段，这里用 empty string 测试
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/stop")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        // 这里会成功解析，但 project_id 为空字符串
        // 实际的验证逻辑在 handler 中，不在 extractor 中
        assert_eq!(response.status(), StatusCode::OK);
    }

    // ==================== 原有测试 ====================

    #[tokio::test]
    async fn test_i18n_json_rejection_returns_bad_request() {
        let app = Router::new().route("/json", post(json_handler));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/json")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"value":"oops"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_i18n_query_rejection_returns_bad_request() {
        let app = Router::new().route("/query", get(query_handler));

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/query?limit=abc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_i18n_path_rejection_returns_bad_request() {
        let app = Router::new().route("/path/{id}", get(path_handler));

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/path/not-a-number")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
