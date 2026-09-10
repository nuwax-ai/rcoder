//! HTTP 提取器适配层：把 Axum 原生 rejection 统一映射为 file-server `AppError`。
//!
//! 另含两个请求级 task-local 标记（中间件 scope 注入）：
//! - userApp 分流标记（`USERAPP_FLAG`）：`X-Service-Type: userapp` 请求经
//!   反向代理/rcoder 拦截层透传到容器内，由 [`scope_userapp_flag`] 读 header
//!   注入，computer 域 workspace 定位（`ws_path` 等）据此切换到 userApp 开发卷
//!   ——HTTP 层标记，与 ServiceType 枚举（容器编排层）互不相干。
//! - 项目绑定目录（`WORKSPACE_DIR`，对齐 TS f979df7）：`x-workspace-dir`
//!   header 的原始值，由 [`scope_workspace_dir`] 注入；与 body/query 的
//!   `workspaceDir` 字段经 [`merged_workspace_dir`] 合并（header 优先）后由
//!   computer 域收口 fail-fast 校验。

use axum::extract::multipart::MultipartRejection;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Multipart, Path, Query, Request};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};

use crate::error::AppError;

// 分流契约常量与 rcoder 转发层共用 shared_types 单一事实源。
pub use shared_types::{
    APP_ID_HEADER, SERVICE_TYPE_HEADER, SERVICE_TYPE_USERAPP, WORKSPACE_DIR_HEADER,
};

// ── 请求级 userApp 分流标记 (task_local, 由请求中间件 scope 注入) ────────────────
tokio::task_local! {
    pub(crate) static USERAPP_FLAG: bool;
    /// userApp 定位的独立 app_id（header `x-app-id` 优先，缺省 query `appId`
    /// 兜底；原始值存储，合法性由定位收口 `resolve_userapp_dev` 校验——与
    /// WORKSPACE_DIR 同款「中间件存原始值、收口 fail-fast」模式）。
    pub(crate) static USERAPP_APP_ID: Option<String>;
}

/// 当前请求是否为 userApp 场景（`X-Service-Type: userapp`；task_local 未设置时 false）。
pub fn is_userapp_request() -> bool {
    USERAPP_FLAG.try_with(|f| *f).unwrap_or(false)
}

/// 当前请求的独立 app_id（对齐 TS `resolveServiceContext` 的两级提取：
/// header `x-app-id` > query `appId`）。非 userApp 请求或两处皆缺 → None。
///
/// cId（会话字段）**不参与** userapp 定位——app_id 是独立字段，缺失由
/// computer 域收口 fail-fast（勿让 cId 兼任，语义违例先例）。
pub fn userapp_app_id() -> Option<String> {
    USERAPP_APP_ID
        .try_with(|v| v.clone())
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
}

/// 中间件：读 `X-Service-Type` header → task-local scope 注入 userApp 标记，
/// 同时提取定位用 app_id（header `x-app-id` 优先，缺省 query `appId` 兜底——
/// Java 静态文件族 query 恒带 `appId`）。query 值不做 percent-decode：
/// app_id 是 identifier 字符集，含转义序列会在定位收口校验 fail-fast。
pub async fn scope_userapp_flag(req: Request, next: axum::middleware::Next) -> Response {
    let is_userapp = req
        .headers()
        .get(SERVICE_TYPE_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(shared_types::is_userapp_service_type_value);
    let app_id = req
        .headers()
        .get(APP_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| query_param_raw(req.uri().query(), "appId"));
    let fut = next.run(req);
    async {
        USERAPP_APP_ID
            .scope(app_id, async { USERAPP_FLAG.scope(is_userapp, fut).await })
            .await
    }
    .await
}

/// 从原始 query 串取单值参数（`appId=19&x=y` → `Some("19")`；多值取首个）。
fn query_param_raw(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key && !v.is_empty()).then(|| v.to_string())
    })
}

// ── 请求级绑定目录 (task_local, 存原始 header 值; 对齐 TS f979df7) ───────────────
tokio::task_local! {
    pub(crate) static WORKSPACE_DIR: Option<String>;
}

/// 当前请求 `x-workspace-dir` header 的**原始值**（未校验；中间件外恒 None）。
///
/// 校验/归一不在此处做——由收口 [`crate::handlers::computer`]
/// `computer_root_for_request` 经 [`merged_workspace_dir`] 合并后 fail-fast
/// （对齐 TS：`resolveServiceContext` 在路由入口校验，非 userapp 域路由不受影响）。
pub fn workspace_dir_header_raw() -> Option<String> {
    WORKSPACE_DIR.try_with(|v| v.clone()).ok().flatten()
}

/// 中间件：读 `x-workspace-dir` header → task-local scope 注入原始值
/// （header 缺失也 scope `None`，与 `scope_userapp_flag` 恒 scope 形态一致；
/// 对不消费绑定目录的路由是 no-op）。
pub async fn scope_workspace_dir(req: Request, next: axum::middleware::Next) -> Response {
    let raw = req
        .headers()
        .get(WORKSPACE_DIR_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    WORKSPACE_DIR.scope(raw, next.run(req)).await
}

/// 合并绑定目录来源（对齐 TS `resolveServiceContext` 的取值顺序）：
/// header（trim 非空）> 调用方显式值（body/query 的 `workspaceDir` 字段，trim 非空）。
///
/// header 为空白串时落回显式值；两者皆空/缺失 → `None`（未绑定，走默认定位）。
/// 只做 trim/非空过滤，合法性校验（绝对路径/点段/长度/控制字符）由收口处
/// [`crate::workspace::normalize_workspace_dir`] fail-fast。
pub fn merged_workspace_dir(explicit: Option<&str>) -> Option<String> {
    if let Some(header) = workspace_dir_header_raw().filter(|s| !s.trim().is_empty()) {
        return Some(header);
    }
    explicit
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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
        for variant in ["Userapp", "USERAPP"] {
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

    /// userapp app_id 两级提取（对齐 TS resolveServiceContext）：
    /// header `x-app-id` 优先 > query `appId` 兜底 > None（缺失由定位收口 fail-fast）。
    #[tokio::test]
    async fn userapp_app_id_extracted_header_first_then_query() {
        let app = Router::new()
            .route(
                "/probe",
                post(|| async { format!("{:?}", super::userapp_app_id()) }),
            )
            .layer(axum::middleware::from_fn(super::scope_userapp_flag));

        // 仅 header
        let resp = app
            .clone()
            .oneshot(
                Request::post("/probe")
                    .header(super::SERVICE_TYPE_HEADER, super::SERVICE_TYPE_USERAPP)
                    .header(super::APP_ID_HEADER, "app-19")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            to_bytes(resp.into_body(), 1024).await.unwrap(),
            r#"Some("app-19")"#
        );

        // header 优先于 query（两处都有 → header 胜出）
        let resp = app
            .clone()
            .oneshot(
                Request::post("/probe?appId=from-query")
                    .header(super::SERVICE_TYPE_HEADER, super::SERVICE_TYPE_USERAPP)
                    .header(super::APP_ID_HEADER, "from-header")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            to_bytes(resp.into_body(), 1024).await.unwrap(),
            r#"Some("from-header")"#
        );

        // 仅 query 兜底（Java 静态文件族形态：query 恒带 appId）
        let resp = app
            .clone()
            .oneshot(
                Request::post("/probe?appId=19&other=1")
                    .header(super::SERVICE_TYPE_HEADER, super::SERVICE_TYPE_USERAPP)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            to_bytes(resp.into_body(), 1024).await.unwrap(),
            r#"Some("19")"#
        );

        // 两处皆缺 → None；空白 header 值视为未携带（回落 query）
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
        assert_eq!(to_bytes(resp.into_body(), 1024).await.unwrap(), "None");
        let resp = app
            .oneshot(
                Request::post("/probe?appId=q1")
                    .header(super::SERVICE_TYPE_HEADER, super::SERVICE_TYPE_USERAPP)
                    .header(super::APP_ID_HEADER, "   ")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            to_bytes(resp.into_body(), 1024).await.unwrap(),
            r#"Some("q1")"#
        );
    }

    /// 绑定目录 task-local：中间件 scope 注入原始 header 值（可读、未校验），
    /// header 缺失时 scope None（handler 内 None）；中间件外恒 None。
    #[tokio::test]
    async fn workspace_dir_task_local_scoped_by_middleware() {
        use super::{merged_workspace_dir, scope_workspace_dir, workspace_dir_header_raw};

        let app = Router::new().route(
            "/probe",
            post(|| async {
                format!(
                    "{:?}|{:?}",
                    workspace_dir_header_raw(),
                    merged_workspace_dir(Some("/from-query"))
                )
            }),
        );
        let app = app.layer(axum::middleware::from_fn(scope_workspace_dir));

        // 带 x-workspace-dir → 原始值可读 + merge 时压过显式值
        let resp = app
            .clone()
            .oneshot(
                Request::post("/probe")
                    .header(super::WORKSPACE_DIR_HEADER, "/bound/dir")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            to_bytes(resp.into_body(), 1024).await.unwrap(),
            r#"Some("/bound/dir")|Some("/bound/dir")"#
        );

        // header 为空白串 → scope Some(空白) 但 merge 落回显式值（对齐 TS）
        let resp = app
            .clone()
            .oneshot(
                Request::post("/probe")
                    .header(super::WORKSPACE_DIR_HEADER, "   ")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            to_bytes(resp.into_body(), 1024).await.unwrap(),
            r#"Some("   ")|Some("/from-query")"#
        );

        // 无 header → scope None + merge 取显式值
        let resp = app
            .oneshot(Request::post("/probe").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            to_bytes(resp.into_body(), 1024).await.unwrap(),
            r#"None|Some("/from-query")"#
        );
    }

    /// merge 优先级（对齐 TS resolveServiceContext：header > body/query > 无）。
    /// 显式值同样 trim 过滤：空白显式值视为未传。
    #[test]
    fn merged_workspace_dir_filters_empty_explicit() {
        use super::merged_workspace_dir;
        assert_eq!(merged_workspace_dir(Some("   ")), None);
        assert_eq!(
            merged_workspace_dir(Some("  /bound  ")),
            Some("/bound".to_string())
        );
        // 中间件外（无 task_local）显式为空 → None（不 panic）
        assert_eq!(merged_workspace_dir(None), None);
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
