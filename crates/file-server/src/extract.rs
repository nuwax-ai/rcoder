//! HTTP 提取器适配层：把 Axum 原生 rejection 统一映射为 file-server `AppError`。
//!
//! 另含请求级 task-local 标记（中间件 scope 注入）：
//! - 服务场景类型（`SERVICE_KIND`）：`X-Service-Type` header 值经
//!   `normalize_computer_service_type` 归一后的四值类型（userApp 切开发卷 /
//!   normalProject 共享工作区 / pageApp·taskAgent 默认）——由
//!   [`scope_service_context`] 读 header 注入，computer 域 workspace 定位
//!   （`ws_path` 等）据此分派——HTTP 层标记，与 ServiceType 枚举（容器编排层）
//!   互不相干。
//! - 用户维度工作目录（`WORKSPACE_PATH`，对齐 TS 1.4.5）：`x-workspace-path`
//!   header 的原始值，由 [`scope_workspace_path`] 注入；与 body/query 的
//!   `workspacePath` 字段经 [`merged_workspace_path`] 合并（header 优先）后由
//!   computer 域收口 fail-fast 校验。

use axum::extract::multipart::MultipartRejection;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Multipart, Path, Query, Request};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};

use crate::error::AppError;

// 分流契约常量与 rcoder 转发层共用 shared_types 单一事实源。
pub use shared_types::{
    APP_ID_HEADER, ComputerServiceKind, SERVICE_TYPE_HEADER, SERVICE_TYPE_USERAPP,
    WORKSPACE_PATH_HEADER,
};

// ── 请求级服务场景上下文 (task_local, 由请求中间件 scope 注入) ──────────────────
tokio::task_local! {
    /// `X-Service-Type` 归一化后的服务场景类型（恒 scope：header 缺失/未匹配
    /// 值 scope `None`——对齐 TS `resolveServiceContext` 的 `headerType ||
    /// bodyType || 缺省` 链：header 未匹配视为无值，轮到 body/query 通道，
    /// 全缺失由消费方按缺省 taskAgent 布局处理）。
    pub(crate) static SERVICE_KIND: Option<ComputerServiceKind>;
    /// 定位的独立 app_id（header `x-app-id` 优先，缺省 query `appId`
    /// 兜底；原始值存储，合法性由定位收口 `resolve_userapp_dev` 校验——与
    /// WORKSPACE_PATH 同款「中间件存原始值、收口 fail-fast」模式）。
    pub(crate) static USERAPP_APP_ID: Option<String>;
}

/// 当前请求 header 通道的服务场景类型（`None` = header 缺失/未匹配，
/// 由消费方决定 body/query 兜底与缺省布局）。
pub fn service_kind() -> Option<ComputerServiceKind> {
    SERVICE_KIND.try_with(|kind| *kind).ok().flatten()
}

/// 当前请求是否为 userApp 场景（`X-Service-Type: userapp`）。
pub fn is_userapp_request() -> bool {
    service_kind() == Some(ComputerServiceKind::Userapp)
}

/// 当前请求是否为 normalProject 场景（`X-Service-Type: normalProject`，
/// 常规项目主容器共享工作区 `{CWS}/{userId}/NormalProject/{projectId}`）。
pub fn is_normal_project_request() -> bool {
    service_kind() == Some(ComputerServiceKind::NormalProject)
}

/// 当前请求的独立 app_id（对齐 TS `resolveServiceContext` 的两级提取：
/// header `x-app-id` > query `appId`）。非 userApp/normalProject 请求或两处
/// 皆缺 → None。
///
/// cId（会话字段）**不参与**定位——app_id 是独立字段，缺失由 computer 域
/// 收口 fail-fast（勿让 cId 兼任，语义违例先例）。
pub fn userapp_app_id() -> Option<String> {
    USERAPP_APP_ID
        .try_with(|v| v.clone())
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
}

/// 中间件：读 `X-Service-Type` header → 归一化为四值类型 scope 注入
/// （缺失/未匹配 scope `None`——header 通道视为无值，对齐 TS `headerType ||
/// bodyType` 链；缺省 taskAgent 由消费方处理，词表终态无 general 兼容），
/// 同时提取定位用 app_id（header `x-app-id` 优先，缺省 query `appId` 兜底——
/// Java 静态文件族 query 恒带 `appId`）。query 值不做 percent-decode：
/// app_id 是 identifier 字符集，含转义序列会在定位收口校验 fail-fast。
pub async fn scope_service_context(req: Request, next: axum::middleware::Next) -> Response {
    let kind = req
        .headers()
        .get(SERVICE_TYPE_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(shared_types::normalize_computer_service_type);
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
            .scope(app_id, async { SERVICE_KIND.scope(kind, fut).await })
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

// ── 请求级用户维度工作目录 (task_local, 存原始 header 值; 对齐 TS 1.4.5) ────────
tokio::task_local! {
    pub(crate) static WORKSPACE_PATH: Option<String>;
}

/// 当前请求 `x-workspace-path` header 的**原始值**（未校验；中间件外恒 None）。
///
/// 校验/归一不在此处做——由收口 [`crate::handlers::computer`]
/// `computer_root_for_request` 经 [`merged_workspace_path`] 合并后 fail-fast
/// （对齐 TS：`resolveServiceContext` 在路由入口校验，非 computer 域路由不受影响）。
pub fn workspace_path_header_raw() -> Option<String> {
    WORKSPACE_PATH.try_with(|v| v.clone()).ok().flatten()
}

/// 中间件：读 `x-workspace-path` header → task-local scope 注入原始值
/// （header 缺失也 scope `None`，与 `scope_service_context` 恒 scope 形态一致；
/// 对不消费该目录的路由是 no-op）。
pub async fn scope_workspace_path(req: Request, next: axum::middleware::Next) -> Response {
    let raw = req
        .headers()
        .get(WORKSPACE_PATH_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    WORKSPACE_PATH.scope(raw, next.run(req)).await
}

/// 合并用户维度工作目录来源（对齐 TS `resolveServiceContext` 的取值顺序）：
/// header（trim 非空）> 调用方显式值（body/query 的 `workspacePath` 字段，trim 非空）。
///
/// header 为空白串时落回显式值；两者皆空/缺失 → `None`（未传入，走默认定位）。
/// 只做 trim/非空过滤，合法性校验（绝对路径/点段/长度/控制字符）由收口处
/// [`crate::workspace::normalize_workspace_path`] fail-fast。
pub fn merged_workspace_path(explicit: Option<&str>) -> Option<String> {
    if let Some(header) = workspace_path_header_raw().filter(|s| !s.trim().is_empty()) {
        return Some(header);
    }
    non_empty_trimmed(explicit).map(str::to_string)
}

/// trim 后非空才保留（对齐 TS `String(x).trim() || null` 的取值语义——
/// 空白串视为未传）。serviceContext 三要素（appId/workspacePath）与定位
/// 收口的 trim 过滤统一走此 helper，防止「空白值假激活」与 TS 分歧。
pub(crate) fn non_empty_trimmed(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
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

    /// task-local 服务场景类型：中间件 scope 注入后 handler 内可读；
    /// userapp 判定只命中 userapp 值，normalProject 命中 normalProject，
    /// 未知值/无 header 落缺省 TaskAgent（对齐 TS 1.4.5 归一终态）。
    #[tokio::test]
    async fn service_kind_scoped_by_middleware() {
        let probe = Router::new().route(
            "/probe",
            post(|| async {
                format!(
                    "{}|{}",
                    is_userapp_request(),
                    super::is_normal_project_request()
                )
            }),
        );
        let app = probe.layer(axum::middleware::from_fn(super::scope_service_context));

        // 带 X-Service-Type: userapp → handler 内 userapp=true
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
        assert_eq!(
            to_bytes(resp.into_body(), 1024).await.unwrap(),
            "true|false"
        );

        // 大小写不敏感变体（单一事实源 shared_types::normalize_computer_service_type）
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
                "true|false",
                "{variant}"
            );
        }

        // normalProject → userapp=false、normalProject=true（驼峰与全小写均命中）
        for variant in ["normalProject", "NormalProject", "normalproject"] {
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
                "false|true",
                "{variant}"
            );
        }

        // 无 header / 未匹配值（含 general 旧值）→ 双 false（缺省 TaskAgent 档）
        for case in ["none", "general", "unknown-x"] {
            let mut req = Request::post("/probe").body(Body::empty()).unwrap();
            if case != "none" {
                req.headers_mut().insert(
                    super::SERVICE_TYPE_HEADER,
                    case.parse().expect("static header value"),
                );
            }
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(
                to_bytes(resp.into_body(), 1024).await.unwrap(),
                "false|false",
                "{case}"
            );
        }
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
            .layer(axum::middleware::from_fn(super::scope_service_context));

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

    /// 用户维度工作目录 task-local：中间件 scope 注入原始 header 值（可读、未校验），
    /// header 缺失时 scope None（handler 内 None）；中间件外恒 None。
    #[tokio::test]
    async fn workspace_path_task_local_scoped_by_middleware() {
        use super::{merged_workspace_path, scope_workspace_path, workspace_path_header_raw};

        let app = Router::new().route(
            "/probe",
            post(|| async {
                format!(
                    "{:?}|{:?}",
                    workspace_path_header_raw(),
                    merged_workspace_path(Some("/from-query"))
                )
            }),
        );
        let app = app.layer(axum::middleware::from_fn(scope_workspace_path));

        // 带 x-workspace-path → 原始值可读 + merge 时压过显式值
        let resp = app
            .clone()
            .oneshot(
                Request::post("/probe")
                    .header(super::WORKSPACE_PATH_HEADER, "/bound/dir")
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
                    .header(super::WORKSPACE_PATH_HEADER, "   ")
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
    fn merged_workspace_path_filters_empty_explicit() {
        use super::merged_workspace_path;
        assert_eq!(merged_workspace_path(Some("   ")), None);
        assert_eq!(
            merged_workspace_path(Some("  /bound  ")),
            Some("/bound".to_string())
        );
        // 中间件外（无 task_local）显式为空 → None（不 panic）
        assert_eq!(merged_workspace_path(None), None);
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
