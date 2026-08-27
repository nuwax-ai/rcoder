//! 应用日志 handler（sources/query/stream，转发到 app 容器内 app-cli :3010）。
//!
//! 三条接口支持 `{app_stage}` 显式环境分派（prod=运行容器实例 IP / dev=开发容器
//! host 重拼 :3010，见 `AppService::log_api_base`）。
//!
//! JSON 转发（sources/query、query）是**透明代理**：透传 app-cli 的状态码 +
//! 三条接口均支持 `{app_stage}` 显式环境分派（prod=运行容器实例 IP / dev=开发容器
//! host 重拼 :3010，见 [`AppService::log_api_base`]）——
//! 响应体——app-cli 侧统一 `HttpResult` 信封（`{code,message,data,tid,success}`），
//! 成功失败都以信封直达调用方，code/message 保真不二次包装；仅连接/读取失败
//! 由 rcoder 生成自己的 HttpResult 错误（AppError 路径）。SSE（stream）豁免信封。
//!
//! wire DTO 与 app-cli 同源（`shared_types::app_cli_logs`），文档响应 schema
//! 因此可见具体字段定义。

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{Response, StatusCode, header};
use futures_util::TryStreamExt;
use serde::Deserialize;
use shared_types::{AppError, HttpResult, LogQueryRequest, LogQueryResponse, LogSourceInfo};

use crate::models::AppOperationError;

use super::AppManagerState;

/// logs 三接口共用的 query 定位参数。
///
/// `parameter_in` 必须显式声明：utoipa-axum 自动发现会按 Path extractor 把
/// query 字段误标 path（项目既有约定），容器级显式声明优先。
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LogsAccessParams {
    /// 归属用户 ID（必填，白名单校验；dev 容器懒创建时宿主树
    /// `dev/{user_id}/{app_id}` 分区依据）
    pub user_id: String,
}

/// 查询应用日志源
///
/// 应用声明的日志源与匹配到的日志文件（转发 app-cli /v1/logs/sources/query）。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{app_stage}/logs/sources/query",
    params(("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserAppBuilder）；`prod`=运行容器（UserApp）")
    ),
    description = r#"
查询应用声明的日志源及匹配到的日志文件清单（选日志面板"源选择器"用）。
`app_stage` 决定目标容器：dev=开发容器的实时源 / prod=运行容器的应用日志源；
请求体 selectors 支持 per-service 过滤（空 = 全量声明面）。
"#,
    responses(
        (
            status = 200,
            description = "查询成功（HttpResult 信封 data=声明日志源与匹配文件列表）",
            body = HttpResult<Vec<LogSourceInfo>>
        ),
        (status = 400, description = "app-cli 拒绝请求（参数错误，信封透传）", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 409, description = "应用无就绪实例 IP（未运行/未就绪），无法访问日志", body = HttpResult<String>),
        (status = 500, description = "连接 app-cli / 响应读取失败", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 日志"
)]
pub async fn query_app_log_sources(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    Query(params): Query<LogsAccessParams>,
    Json(request): Json<LogQueryRequest>,
) -> Result<Response<Body>, AppError> {
    let app_stage = super::parse_app_stage_param(&app_stage)?;
    shared_types::validate_identifier(&params.user_id, "user_id")
        .map_err(|e| AppError::validation_error(&e))?;
    let base = state
        .app_service
        .log_api_base(app_stage, &app_id, &params.user_id)
        .await?;
    forward_json(&state, base.clone(), "/v1/logs/sources/query", request).await
}

/// 查询应用日志快照
///
/// 多服务日志快照，带 checkpoint 游标支持增量拉取（转发 app-cli /v1/logs/query）。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{app_stage}/logs/query",
    params(("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserAppBuilder）；`prod`=运行容器（UserApp）")
    ),
    description = r#"
多服务日志快照（分页拉取，非 SSE）：携带上次响应的 `cursor` 即可断点续拉；
`cursor_reset=true` 表示跨部署代需从 tail 重读。`app_stage` 选择目标容器同
sources/query。
"#,
    responses(
        (
            status = 200,
            description = "查询成功（HttpResult 信封 data=多服务日志快照；cursor 回填下次请求可断点续拉）",
            body = HttpResult<LogQueryResponse>
        ),
        (status = 400, description = "app-cli 拒绝请求（参数错误，信封透传）", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 409, description = "应用无就绪实例 IP（未运行/未就绪），无法访问日志", body = HttpResult<String>),
        (status = 500, description = "连接 app-cli / 响应读取失败", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 日志"
)]
pub async fn query_app_logs(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    Query(params): Query<LogsAccessParams>,
    Json(request): Json<LogQueryRequest>,
) -> Result<Response<Body>, AppError> {
    let app_stage = super::parse_app_stage_param(&app_stage)?;
    shared_types::validate_identifier(&params.user_id, "user_id")
        .map_err(|e| AppError::validation_error(&e))?;
    let base = state
        .app_service
        .log_api_base(app_stage, &app_id, &params.user_id)
        .await?;
    forward_json(&state, base, "/v1/logs/query", request).await
}

/// 实时日志 SSE 流
///
/// 转发 app-cli /v1/logs/stream，Content-Type: text/event-stream。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/{app_stage}/logs/stream",
    params(("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserAppBuilder）；`prod`=运行容器（UserApp）")
    ),
    description = r#"
SSE 实时日志流（500ms 轮询内核）：事件清单与断线续传协议见 200 响应说明。
`app_stage` 选择目标容器同 sources/query；断线后以最近 checkpoint 回填 cursor 重连，
部署代切换收 `cursor_reset` 后重置游标。
"#,
    responses(
        (
            status = 200,
            description = "SSE 实时日志流（转发容器内 app-cli，轮询周期 500ms；首轮带 tail 默认 100 行，后续增量）。每条消息 `event:<事件名>` + `data:<JSON>`，字段 snake_case。\n\n事件清单：\n- `log` → 日志行：`{'service_id':'web','source_id':'runtime','file':'web.log','offset':123,'timestamp':'...','level':'INFO','message':'一行日志'}`（timestamp/level 可空，文本格式日志无时间戳解析）\n- `source_error` → 某日志源读取失败：`{'service_id':'...','source_id':'...','code':'...','message':'...'}`（去重：同源只报一次）\n- `source_recovered` → 失败源恢复：`{'service_id':'...','source_id':'...'}`\n- `cursor_reset` → 游标失效（跨部署代/游标损坏）：`{'message':'...'}`，客户端应丢弃本地 cursor 从 tail 重新开始\n- `checkpoint` → data 为新游标字符串（base64，可直接回填请求体 cursor 断线续传）\n- `heartbeat` → 保活（每 15s），data='{}'\n\n断线续传：把最近一次 checkpoint 的值作为请求体 cursor 重发即可从断点继续；重新部署后游标代际变化会收到 cursor_reset。",
            content_type = "text/event-stream",
        ),
        (status = 400, description = "app-cli 拒绝请求（参数错误）", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 409, description = "应用无就绪实例 IP（未运行/未就绪），无法访问日志", body = HttpResult<String>),
        (status = 500, description = "连接 app-cli / 建流失败", body = HttpResult<String>)
    ),
    tag = "UserApp · 双态 · 日志"
)]
pub async fn stream_app_logs_v1(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, app_stage)): Path<(String, String)>,
    Query(params): Query<LogsAccessParams>,
    Json(request): Json<LogQueryRequest>,
) -> Result<Response<Body>, AppError> {
    let app_stage = super::parse_app_stage_param(&app_stage)?;
    shared_types::validate_identifier(&params.user_id, "user_id")
        .map_err(|e| AppError::validation_error(&e))?;
    let base = state
        .app_service
        .log_api_base(app_stage, &app_id, &params.user_id)
        .await?;
    let response = state
        .http_client
        .post(format!("{base}/v1/logs/stream"))
        .json(&request)
        .send()
        .await
        .map_err(|error| backend(format!("connect to app-cli log stream: {error}")))?;
    if !response.status().is_success() {
        let status = response.status();
        let message = response.text().await.unwrap_or_default();
        if status.is_client_error() {
            return Err(AppOperationError::Validation(format!(
                "app-cli rejected log stream ({status}): {message}"
            ))
            .into());
        }
        return Err(backend(format!("app-cli log stream failed ({status}): {message}")).into());
    }
    let stream = response.bytes_stream().map_err(std::io::Error::other);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .map_err(|error| backend(format!("build SSE response: {error}")).into())
}

/// 透明代理：透传 app-cli 的状态码 + 响应体（含 Content-Type）。app-cli 侧
/// 成功失败都是 HttpResult 信封，直达调用方不二次包装；仅连接/读取失败
/// 走 rcoder 自己的 AppError→HttpResult 错误。
async fn forward_json(
    state: &Arc<AppManagerState>,
    base: String,
    path: &str,
    request: LogQueryRequest,
) -> Result<Response<Body>, AppError> {
    let response = state
        .http_client
        .post(format!("{base}{path}"))
        .json(&request)
        .send()
        .await
        .map_err(|error| {
            AppError::from(backend(format!("connect to app-cli logs API: {error}")))
        })?;
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let body = response
        .bytes()
        .await
        .map_err(|error| AppError::from(backend(format!("read app-cli logs response: {error}"))))?;
    let mut builder = Response::builder().status(status);
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    builder
        .body(Body::from(body))
        .map_err(|error| AppError::from(backend(format!("build forwarded log response: {error}"))))
}

fn backend(message: String) -> AppOperationError {
    AppOperationError::Backend(message)
}
