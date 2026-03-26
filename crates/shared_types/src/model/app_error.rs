use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("anyhow::Error: {0}")]
    AnyhowError(#[from] anyhow::Error),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("AppError(code={code}, internal={internal_message:?}, i18n_key={i18n_key:?})")]
    Structured {
        code: String,
        internal_message: Option<String>,
        i18n_key: Option<String>,
    },
}

impl AppError {
    /// Create a generic error from a string
    pub fn generic(msg: impl Into<String>) -> Self {
        Self::with_message(crate::error_codes::ERR_INTERNAL_SERVER_ERROR, msg)
    }

    /// 通过错误码创建结构化错误
    pub fn from_code(code: &str) -> Self {
        Self::Structured {
            code: code.to_string(),
            internal_message: None,
            i18n_key: None,
        }
    }

    /// 通过错误码和内部信息创建结构化错误
    pub fn with_message(code: &str, msg: impl Into<String>) -> Self {
        Self::Structured {
            code: code.to_string(),
            internal_message: Some(msg.into()),
            i18n_key: None,
        }
    }

    /// 通过错误码和 i18n key 创建结构化错误
    pub fn with_i18n_key(code: &str, i18n_key: &str) -> Self {
        Self::Structured {
            code: code.to_string(),
            internal_message: None,
            i18n_key: Some(i18n_key.to_string()),
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
}

// 为 axum 实现 IntoResponse trait
impl axum::response::IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let locale = crate::current_request_locale();

        let (code, internal_message, i18n_key) = match self {
            AppError::AnyhowError(e) => (
                crate::error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
                Some(e.to_string()),
                None,
            ),
            AppError::IoError(e) => (
                crate::error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
                Some(e.to_string()),
                None,
            ),
            AppError::Structured {
                code,
                internal_message,
                i18n_key,
            } => (code, internal_message, i18n_key),
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

        let response = if let Some(key) = i18n_key {
            crate::HttpResult::<String>::error_with_message(
                &code,
                locale,
                &crate::get_i18n_message(&key, locale),
            )
        } else {
            crate::HttpResult::<String>::error_with_locale(&code, locale)
        };

        (status, axum::Json(response)).into_response()
    }
}

fn status_from_code(code: &str) -> axum::http::StatusCode {
    match code {
        crate::error_codes::ERR_VALIDATION | crate::error_codes::ERR_INVALID_PARAMS => {
            axum::http::StatusCode::BAD_REQUEST
        }
        crate::error_codes::ERR_API_KEY_AUTH_FAILED => axum::http::StatusCode::UNAUTHORIZED,
        crate::error_codes::ERR_TOO_MANY_REQUESTS => axum::http::StatusCode::TOO_MANY_REQUESTS,
        crate::error_codes::ERR_SESSION_NOT_FOUND | crate::error_codes::ERR_CONTAINER_NOT_FOUND => {
            axum::http::StatusCode::NOT_FOUND
        }
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

        assert_eq!(
            response.status(),
            axum::http::StatusCode::BAD_REQUEST
        );
    }
}
