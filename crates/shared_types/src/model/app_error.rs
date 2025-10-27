use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("anyhow::Error: {0}")]
    AnyhowError(#[from] anyhow::Error),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Generic error: {0}")]
    Generic(String),
}

impl AppError {
    /// Create a generic error from a string
    pub fn generic(msg: impl Into<String>) -> Self {
        AppError::Generic(msg.into())
    }

    /// Create an internal server error
    pub fn internal_server_error(msg: &str) -> Self {
        AppError::Generic(format!("Internal server error: {}", msg))
    }
}

// 为 axum 实现 IntoResponse trait
impl axum::response::IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (status, error_message) = match self {
            AppError::AnyhowError(e) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Internal error: {}", e)
            ),
            AppError::IoError(e) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("IO error: {}", e)
            ),
            AppError::Generic(msg) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                msg.clone()
            ),
        };

        let body = axum::Json(serde_json::json!({
            "success": false,
            "error": {
                "code": "INTERNAL_ERROR",
                "message": error_message
            }
        }));

        (status, body).into_response()
    }
}

// 添加从 tokio mpsc SendError 的 From 实现 (用于agent_runner中的mpsc错误)
impl<T> From<tokio::sync::mpsc::error::SendError<T>> for AppError {
    fn from(error: tokio::sync::mpsc::error::SendError<T>) -> Self {
        AppError::Generic(format!("Send error: {}", error))
    }
}