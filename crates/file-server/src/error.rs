//! 统一错误类型 (对齐 nuwax `utils/error/errorHandler.js`)。
//!
//! 错误响应体:
//! ```jsonc
//! { "success": false, "code": "<TYPE>",
//!   "error": { "type": "<TYPE>", "message": "...", "timestamp": "<CST>", "requestId": "..."[, "details": {}] } }
//! ```
//! HTTP 状态码由错误类型决定 (见 [`AppError::status_code`])。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

/// 业务错误, 对应 nuwax 的 AppError 子类族。
#[derive(Debug)]
pub enum AppError {
    /// VALIDATION_ERROR (400)
    Validation(String, Option<Value>),
    /// BUSINESS_ERROR (400)
    Business(String),
    /// PERMISSION_ERROR (403)
    Permission(String),
    /// RESOURCE_ERROR (404)
    Resource(String),
    /// NETWORK_ERROR (502)
    Network(String),
    /// SYSTEM_ERROR (500)
    System(String),
    /// FILE_ERROR (500)
    File(String),
    /// PROCESS_ERROR (500)
    Process(String),
}

impl AppError {
    pub fn validation(msg: impl Into<String>) -> Self {
        AppError::Validation(msg.into(), None)
    }
    pub fn validation_with(msg: impl Into<String>, details: Value) -> Self {
        AppError::Validation(msg.into(), Some(details))
    }
    pub fn business(msg: impl Into<String>) -> Self {
        AppError::Business(msg.into())
    }
    pub fn permission(msg: impl Into<String>) -> Self {
        AppError::Permission(msg.into())
    }
    pub fn resource(msg: impl Into<String>) -> Self {
        AppError::Resource(msg.into())
    }
    pub fn network(msg: impl Into<String>) -> Self {
        AppError::Network(msg.into())
    }
    pub fn system(msg: impl Into<String>) -> Self {
        AppError::System(msg.into())
    }
    pub fn file(msg: impl Into<String>) -> Self {
        AppError::File(msg.into())
    }
    pub fn process(msg: impl Into<String>) -> Self {
        AppError::Process(msg.into())
    }

    fn type_name(&self) -> &'static str {
        match self {
            AppError::Validation(..) => "VALIDATION_ERROR",
            AppError::Business(_) => "BUSINESS_ERROR",
            AppError::Permission(_) => "PERMISSION_ERROR",
            AppError::Resource(_) => "RESOURCE_ERROR",
            AppError::Network(_) => "NETWORK_ERROR",
            AppError::System(_) => "SYSTEM_ERROR",
            AppError::File(_) => "FILE_ERROR",
            AppError::Process(_) => "PROCESS_ERROR",
        }
    }

    fn status_code(&self) -> StatusCode {
        match self {
            AppError::Validation(..) | AppError::Business(_) => StatusCode::BAD_REQUEST,
            AppError::Permission(_) => StatusCode::FORBIDDEN,
            AppError::Resource(_) => StatusCode::NOT_FOUND,
            AppError::Network(_) => StatusCode::BAD_GATEWAY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn message(&self) -> &str {
        match self {
            AppError::Validation(m, _) => m,
            AppError::Business(m) => m,
            AppError::Permission(m) => m,
            AppError::Resource(m) => m,
            AppError::Network(m) => m,
            AppError::System(m) => m,
            AppError::File(m) => m,
            AppError::Process(m) => m,
        }
    }

    fn details(&self) -> Option<&Value> {
        match self {
            AppError::Validation(_, d) => d.as_ref(),
            _ => None,
        }
    }
}

/// 当前 CST(UTC+8) RFC3339 时间字符串 (对齐 nuwax errorHandler timestamp)。
fn now_cst_rfc3339() -> String {
    let cst = chrono::Utc::now() + chrono::Duration::hours(8);
    cst.format("%Y-%m-%dT%H:%M:%S%.3f+08:00").to_string()
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let typ = self.type_name();
        let message = self.message().to_string();
        let mut error = json!({
            "type": typ,
            "message": message,
            "timestamp": now_cst_rfc3339(),
            // TODO: 由请求中间件注入 X-Request-Id 并回填
            "requestId": "unknown",
        });
        if let Some(d) = self.details() {
            error["details"] = d.clone();
        }
        let body = json!({ "success": false, "code": typ, "error": error });
        (status, Json(body)).into_response()
    }
}

/// `?` 传播: io 错误 → SystemError。
impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::System(format!("io error: {e}"))
    }
}

pub type AppResult<T> = Result<T, AppError>;
