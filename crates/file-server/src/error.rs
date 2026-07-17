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

/// 当前 CST(UTC+8) 时间字符串 `YYYY/MM/DD HH:MM:SS` (对齐 nuwax getCSTTimestampString)。
fn now_cst_timestamp() -> String {
    let cst = chrono::Utc::now() + chrono::Duration::hours(8);
    cst.format("%Y/%m/%d %H:%M:%S").to_string()
}

// ── 请求级 requestId (task_local, 由请求中间件 scope 注入; error 响应体回填) ──────

tokio::task_local! {
    /// 当前请求的 requestId (对齐 nuwax req.requestId); 未在请求上下文内 → 回退 "unknown"。
    pub static REQUEST_ID: String;
}

/// 取当前请求 requestId (task_local 未设置时回退 "unknown")。
pub fn current_request_id() -> String {
    REQUEST_ID
        .try_with(|v| v.clone())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// 生成 requestId (对齐 nuwax generateRequestId: 短 base36 串; 无 rand 依赖,
/// 用 纳秒 ^ 原子计数器 ^ pid 混合保证单进程内唯一, 供日志/响应关联)。
pub fn generate_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mixed = nanos ^ (n << 12) ^ (std::process::id() as u64).wrapping_mul(2654435761);
    base36(mixed)
}

/// u64 → base36 字符串。
fn base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut s: Vec<u8> = Vec::new();
    while n > 0 {
        s.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_else(|_| "0".to_string())
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
            "timestamp": now_cst_timestamp(),
            "requestId": current_request_id(),
        });
        if let Some(d) = self.details() {
            error["details"] = d.clone();
        }
        // code 恒 "UNKNOWN_ERROR" (对齐 nuwax: AppError 不设 code, formatErrorResponse
        // 兜底 UNKNOWN_ERROR; 具体类型在 error.type)。
        let body = json!({ "success": false, "code": "UNKNOWN_ERROR", "error": error });
        (status, Json(body)).into_response()
    }
}

/// garde 校验错误 (Report) → AppError::validation (对齐 shared_types::garde_err_to_app_error,
/// 但返回 file-server 本地的 AppError)。消息形如 "field: <rule message>; field2: ..."。
pub fn from_garde(report: garde::Report) -> AppError {
    let msg = report
        .iter()
        .map(|(path, err)| format!("{path}: {}", err.message()))
        .collect::<Vec<_>>()
        .join("; ");
    AppError::validation(msg)
}

/// `?` 传播: io 错误按 kind 细分 (对齐 nuwax classifyIoError:
/// NotFound→RESOURCE(404), PermissionDenied→PERMISSION(403),
/// ConnectionRefused→NETWORK(502), 其余→SYSTEM(500))。
impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => AppError::Resource(format!("io error: {e}")),
            std::io::ErrorKind::PermissionDenied => AppError::Permission(format!("io error: {e}")),
            std::io::ErrorKind::ConnectionRefused => AppError::Network(format!("io error: {e}")),
            _ => AppError::System(format!("io error: {e}")),
        }
    }
}

pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_classifies_by_kind() {
        match AppError::from(std::io::Error::from(std::io::ErrorKind::NotFound)) {
            AppError::Resource(_) => {}
            other => panic!("expected Resource, got {other:?}"),
        }
        match AppError::from(std::io::Error::from(std::io::ErrorKind::PermissionDenied)) {
            AppError::Permission(_) => {}
            other => panic!("expected Permission, got {other:?}"),
        }
        match AppError::from(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)) {
            AppError::Network(_) => {}
            other => panic!("expected Network, got {other:?}"),
        }
        match AppError::from(std::io::Error::from(std::io::ErrorKind::AlreadyExists)) {
            AppError::System(_) => {}
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn generate_request_id_is_base36_and_unique() {
        let a = generate_request_id();
        let b = generate_request_id();
        assert!(!a.is_empty());
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        // 连续两次应不同 (计数器递增)
        assert_ne!(a, b);
    }

    #[test]
    fn current_request_id_fallback_without_scope() {
        // task_local 未 scope 时回退 "unknown"
        assert_eq!(current_request_id(), "unknown");
    }
}
