use thiserror::Error;

/// `AppError::Structured` 的载荷（clippy result_large_err 根治：Box 承载
/// 后 AppError 缩到单指针量级——14 个 handler 签名不再触发 128 字节阈值，
/// 错误值移动也不再复制 ~160 字节载荷）。
#[derive(Error, Debug)]
pub enum AppError {
    #[error("anyhow::Error: {0}")]
    AnyhowError(#[from] anyhow::Error),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("{}", .0.display())]
    Structured(Box<StructuredErrorDetail>),
}

/// 结构化错误详情（原 `AppError::Structured` 变体的内联字段——语义不变，
/// 仅由 Box 承载）。
#[derive(Debug)]
pub struct StructuredErrorDetail {
    // 字段在前（构造点按名初始化），Display 实现在下方 impl
    pub code: String,
    pub internal_message: Option<String>,
    pub i18n_key: Option<String>,
    pub operation_id: Option<String>,
    pub blocker: Option<crate::UserAppOperationBlocker>,
}

impl StructuredErrorDetail {
    fn display(&self) -> String {
        format!(
            "AppError(code={}, internal={:?}, i18n_key={:?})",
            self.code, self.internal_message, self.i18n_key
        )
    }
}

impl AppError {
    /// Create a generic error from a string
    pub fn generic(msg: impl Into<String>) -> Self {
        Self::with_message(crate::error_codes::ERR_INTERNAL_SERVER_ERROR, msg)
    }

    /// 通过错误码创建结构化错误
    pub fn from_code(code: &str) -> Self {
        Self::structured(code, None, None)
    }

    /// 通过错误码和内部信息创建结构化错误
    pub fn with_message(code: &str, msg: impl Into<String>) -> Self {
        Self::structured(code, Some(msg.into()), None)
    }

    /// 通过错误码和 i18n key 创建结构化错误
    pub fn with_i18n_key(code: &str, i18n_key: &str) -> Self {
        Self::structured(code, None, Some(i18n_key.to_string()))
    }

    fn structured(code: &str, internal_message: Option<String>, i18n_key: Option<String>) -> Self {
        Self::Structured(Box::new(StructuredErrorDetail {
            code: code.to_string(),
            internal_message,
            i18n_key,
            operation_id: None,
            blocker: None,
        }))
    }

    /// Attach the in-flight operation that blocks the request. Only meaningful
    /// alongside a conflict code; ignored for non-structured variants.
    pub fn with_blocker(self, blocker: crate::UserAppOperationBlocker) -> Self {
        let mut error = self.into_structured();
        error.blocker = Some(blocker);
        Self::Structured(error)
    }

    /// Attach a verified durable operation identity without changing the error code.
    pub fn with_operation_id(self, id: String) -> Self {
        let mut error = self.into_structured();
        error.operation_id = Some(id);
        Self::Structured(error)
    }

    /// 统一升级为结构化形态（非结构化变体转通用错误，保留 Display 全链）。
    fn into_structured(self) -> Box<StructuredErrorDetail> {
        match self {
            Self::Structured(detail) => detail,
            Self::AnyhowError(error) => Box::new(StructuredErrorDetail {
                code: crate::error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
                internal_message: Some(format!("{error:#}")),
                i18n_key: None,
                operation_id: None,
                blocker: None,
            }),
            Self::IoError(error) => Box::new(StructuredErrorDetail {
                code: crate::error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
                internal_message: Some(error.to_string()),
                i18n_key: None,
                operation_id: None,
                blocker: None,
            }),
        }
    }

    /// Create an internal server error
    pub fn internal_server_error(msg: &str) -> Self {
        Self::with_message(crate::error_codes::ERR_INTERNAL_SERVER_ERROR, msg)
    }

    /// Create a validation error
    pub fn validation_error(msg: &str) -> Self {
        Self::with_message(crate::error_codes::ERR_VALIDATION, msg)
    }

    /// Create a not found error
    pub fn not_found(msg: &str) -> Self {
        Self::with_message(crate::error_codes::ERR_NOT_FOUND, msg)
    }

    /// Create a conflict error
    pub fn conflict(msg: &str) -> Self {
        Self::with_message(crate::error_codes::ERR_CONFLICT, msg)
    }

    /// Create a bad request error
    pub fn bad_request(msg: &str) -> Self {
        Self::with_message(crate::error_codes::ERR_VALIDATION, msg)
    }
}

// 为 axum 实现 IntoResponse trait
impl axum::response::IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let locale = crate::current_request_locale();

        let (code, internal_message, i18n_key, operation_id, blocker) = match self {
            AppError::AnyhowError(e) => (
                crate::error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
                // {e:#} = anyhow alternate Display，展开完整因果链（顶层 context → 底层根因）。
                // to_string() 只返回顶层 context，会吞掉底层错误（如 bollard/容器错误），
                // 违反 Fail Fast：调用方/运维只看到 "[APP] create_deployment 失败"，看不到根因。
                Some(format!("{e:#}")),
                None,
                None,
                None,
            ),
            AppError::IoError(e) => (
                crate::error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
                Some(e.to_string()),
                None,
                None,
                None,
            ),
            AppError::Structured(detail) => {
                let StructuredErrorDetail {
                    code,
                    internal_message,
                    i18n_key,
                    operation_id,
                    blocker,
                } = *detail;
                (code, internal_message, i18n_key, operation_id, blocker)
            }
        };
        let status = status_from_code(&code);

        if let Some(ref msg) = internal_message {
            tracing::error!(
                "AppError response: code={}, locale={}, internal_message={}",
                code,
                locale,
                msg
            );
        }

        // 优先使用 internal_message 作为具体错误信息返回给客户端
        let response = if let Some(ref msg) = internal_message {
            crate::HttpResult::<String>::error_with_message(&code, locale, msg)
        } else if let Some(key) = i18n_key {
            crate::HttpResult::<String>::error_with_message(
                &code,
                locale,
                &crate::get_i18n_message(&key, locale),
            )
        } else {
            crate::HttpResult::<String>::error_with_locale(&code, locale)
        };

        let response = match operation_id {
            Some(id) => response.with_operation_id(id),
            None => response,
        };
        let response = match blocker {
            Some(blocker) => response.with_blocker(blocker),
            None => response,
        };
        (status, axum::Json(response)).into_response()
    }
}

fn status_from_code(code: &str) -> axum::http::StatusCode {
    use crate::error_codes as ec;
    match code {
        ec::ERR_VALIDATION | ec::ERR_INVALID_PARAMS | ec::ERR_DEV_NOT_RUNNING => {
            axum::http::StatusCode::BAD_REQUEST
        }
        ec::ERR_API_KEY_AUTH_FAILED => axum::http::StatusCode::UNAUTHORIZED,
        ec::ERR_TOO_MANY_REQUESTS => axum::http::StatusCode::TOO_MANY_REQUESTS,
        ec::ERR_NOT_FOUND
        | ec::ERR_SESSION_NOT_FOUND
        | ec::ERR_CONTAINER_NOT_FOUND
        | ec::ERR_PROJECT_NOT_FOUND
        | ec::ERR_AGENT_MGMT_NOT_FOUND
        | ec::ERR_AGENT_MGMT_UNKNOWN_AGENT
        | ec::ERR_APP_NOT_FOUND
        | ec::ERR_FILE_NOT_FOUND => axum::http::StatusCode::NOT_FOUND,
        ec::ERR_AGENT_MGMT_BUILTIN_PROTECTED => axum::http::StatusCode::FORBIDDEN,
        ec::ERR_AGENT_MGMT_ALREADY_INSTALLED
        | ec::ERR_CONFLICT
        | ec::ERR_APP_ALREADY_EXISTS
        | ec::ERR_INVALID_STATE => axum::http::StatusCode::CONFLICT,
        ec::ERR_AGENT_MGMT_INVALID_MANIFEST
        | ec::ERR_AGENT_MGMT_INVALID_CHUNK
        | ec::ERR_AGENT_MGMT_CHECKSUM_MISMATCH
        | ec::ERR_AGENT_MGMT_PATH_TRAVERSAL
        | ec::ERR_AGENT_MGMT_BINARY_TOO_LARGE
        | ec::ERR_AGENT_MGMT_ARCHIVE_BOMB
        | ec::ERR_AGENT_MGMT_UNSUPPORTED_TYPE
        | ec::ERR_OPERATION_NOT_SUPPORTED => axum::http::StatusCode::BAD_REQUEST,
        ec::ERR_AGENT_MGMT_COMMAND_TIMEOUT | ec::ERR_USERAPP_WAIT_TIMEOUT => {
            axum::http::StatusCode::GATEWAY_TIMEOUT
        }
        ec::ERR_AGENT_MGMT_PERMISSION_DENIED => axum::http::StatusCode::FORBIDDEN,
        ec::ERR_AGENT_MGMT_DISK_FULL => axum::http::StatusCode::INSUFFICIENT_STORAGE,
        ec::ERR_AGENT_MGMT_STREAM_TRUNCATED => axum::http::StatusCode::BAD_REQUEST,
        ec::ERR_IMAGE_PULL_FAILED => axum::http::StatusCode::BAD_GATEWAY,
        ec::ERR_SERVICE_UNAVAILABLE
        | ec::ERR_AGENT_RUNNER_UNAVAILABLE
        | ec::ERR_AGENT_CONTAINER_UNAVAILABLE
        | ec::ERR_PROXY_DISABLED
        | ec::ERR_PROXY_SERVICE_UNAVAILABLE
        | ec::ERR_RESOURCE_EXHAUSTED => axum::http::StatusCode::SERVICE_UNAVAILABLE,
        ec::ERR_BACKEND_ERROR => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        _ => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
    }
}

// 添加从 tokio mpsc SendError 的 From 实现 (用于agent_runner中的mpsc错误)
impl<T> From<tokio::sync::mpsc::error::SendError<T>> for AppError {
    fn from(error: tokio::sync::mpsc::error::SendError<T>) -> Self {
        AppError::with_message(
            crate::error_codes::ERR_INTERNAL_SERVER_ERROR,
            error.to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::AppError;
    use crate::{error_codes::ERR_VALIDATION, scope_request_locale};

    #[tokio::test]
    async fn test_app_error_into_response_uses_locale() {
        let response = scope_request_locale("zh-CN", async {
            axum::response::IntoResponse::into_response(AppError::with_i18n_key(
                ERR_VALIDATION,
                "error.user_id_required",
            ))
        })
        .await;

        assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    /// Pingora 观测接口错误码须映射 503（/proxy/status 等依赖此语义）。
    #[test]
    fn proxy_error_codes_map_to_service_unavailable() {
        assert_eq!(
            super::status_from_code(crate::error_codes::ERR_PROXY_DISABLED),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            super::status_from_code(crate::error_codes::ERR_PROXY_SERVICE_UNAVAILABLE),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
    }

    /// clippy result_large_err 根治验证：AppError 尺寸须低于 128 字节阈值
    /// （Box 承载 Structured 载荷后单指针量级）。
    #[test]
    fn app_error_size_below_large_err_threshold() {
        use std::mem::size_of;
        assert!(
            size_of::<AppError>() < 128,
            "AppError is {} bytes (clippy result_large_err threshold 128)",
            size_of::<AppError>()
        );
    }
}
