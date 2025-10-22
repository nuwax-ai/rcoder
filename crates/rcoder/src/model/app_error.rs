use axum::response::{IntoResponse, Response};
use thiserror::Error;

use crate::HttpResult;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("serde_json::Error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),

    #[error("anyhow::Error: {0}")]
    AnyhowError(#[from] anyhow::Error),

    #[error("tokio::sync::mpsc::error::SendError<LocalSetAgentRequest>: {0}")]
    SendLocalSetAgentRequestError(
        #[from] tokio::sync::mpsc::error::SendError<crate::proxy_agent::LocalSetAgentRequest>,
    ),
}

impl AppError {
    /// 创建内部服务器错误
    pub fn internal_server_error(message: &str) -> Self {
        Self::AnyhowError(anyhow::anyhow!("Internal Server Error: {}", message))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let http_result = HttpResult::<()>::error("0001", &self.to_string());
        http_result.into_response()
    }
}
