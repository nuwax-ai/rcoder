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
    pub error_detail: Option<crate::ErrorDetail>,
    pub operation_in_progress_data: Option<crate::OperationInProgressData>,
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
        Self::structured(code, Some(crate::sanitize_error_text(&msg.into())), None)
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
            error_detail: None,
            operation_in_progress_data: None,
        }))
    }

    /// Attach origin, phase and safe retry evidence without replacing identity.
    pub fn with_error_detail(self, detail: crate::ErrorDetail) -> Self {
        let mut error = self.into_structured();
        error.error_detail = Some(detail.localized(crate::current_request_locale()));
        Self::Structured(error)
    }

    /// Only the typed admission-conflict code may replace error `data`.
    /// Other conflicts and existing failure payloads retain their contract.
    pub fn with_operation_in_progress_data(self, mut data: crate::OperationInProgressData) -> Self {
        let mut error = self.into_structured();
        if error.code == crate::ERR_OPERATION_IN_PROGRESS {
            if error.operation_id.is_some() {
                data.retryable = false;
                data.retry_after_seconds = 0;
            }
            error.operation_in_progress_data = Some(data);
        }
        Self::Structured(error)
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
        if let Some(data) = &mut error.operation_in_progress_data {
            data.retryable = false;
            data.retry_after_seconds = 0;
        }
        Self::Structured(error)
    }

    /// 统一升级为结构化形态（非结构化变体转通用错误，保留 Display 全链）。
    fn into_structured(self) -> Box<StructuredErrorDetail> {
        match self {
            Self::Structured(detail) => detail,
            Self::AnyhowError(error) => Box::new(StructuredErrorDetail {
                code: crate::error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
                internal_message: Some(crate::sanitize_error_text(&format!("{error:#}"))),
                i18n_key: None,
                operation_id: None,
                blocker: None,
                error_detail: None,
                operation_in_progress_data: None,
            }),
            Self::IoError(error) => Box::new(StructuredErrorDetail {
                code: crate::error_codes::ERR_INTERNAL_SERVER_ERROR.to_string(),
                internal_message: Some(crate::sanitize_error_text(&error.to_string())),
                i18n_key: None,
                operation_id: None,
                blocker: None,
                error_detail: None,
                operation_in_progress_data: None,
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

    /// Transport status uses the same structured contract in HTTP and proxy paths.
    pub fn status_code(&self) -> axum::http::StatusCode {
        match self {
            Self::Structured(detail) => status_from_code(&detail.code),
            Self::AnyhowError(_) | Self::IoError(_) => {
                axum::http::StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }

    /// Preserve diagnostics in an entry point whose established transport is
    /// HTTP 200 + HttpResult. Standard AppError responses keep their status.
    pub fn into_http_result<T>(self, locale: &str) -> crate::HttpResult<T> {
        let mut detail = self.into_structured();
        if detail.operation_id.is_some()
            && let Some(data) = &mut detail.operation_in_progress_data
        {
            data.retryable = false;
            data.retry_after_seconds = 0;
        }
        let message = match &detail.operation_in_progress_data {
            Some(data) if detail.code == crate::ERR_OPERATION_IN_PROGRESS => {
                data.localized_message(locale)
            }
            _ => match detail.internal_message {
                Some(message) => crate::sanitize_error_text(&message),
                None => match detail.i18n_key {
                    Some(key) => crate::get_i18n_message(&key, locale),
                    None => crate::get_error_message(&detail.code, locale),
                },
            },
        };
        let mut response = crate::HttpResult::error(&detail.code, &message);
        response.operation_id = detail.operation_id;
        response.blocker = detail.blocker;
        response.error_detail = detail.error_detail.map(|detail| detail.localized(locale));
        response.operation_in_progress_data = detail.operation_in_progress_data;
        response
    }
}

// 为 axum 实现 IntoResponse trait
impl axum::response::IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let locale = crate::current_request_locale();
        let response = self.into_http_result::<String>(locale);
        let status = status_from_code(&response.code);
        tracing::error!(
            code = %response.code,
            locale,
            message = %response.message,
            operation_id = ?response.operation_id,
            "AppError response"
        );
        (status, axum::Json(response)).into_response()
    }
}

pub fn status_from_code(code: &str) -> axum::http::StatusCode {
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
        | ec::ERR_OPERATION_IN_PROGRESS
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
        ec::ERR_AGENT_MGMT_COMMAND_TIMEOUT
        | ec::ERR_USERAPP_WAIT_TIMEOUT
        | ec::ERR_RUNTIME_TIMEOUT => axum::http::StatusCode::GATEWAY_TIMEOUT,
        ec::ERR_AGENT_MGMT_PERMISSION_DENIED => axum::http::StatusCode::FORBIDDEN,
        ec::ERR_AGENT_MGMT_DISK_FULL => axum::http::StatusCode::INSUFFICIENT_STORAGE,
        ec::ERR_AGENT_MGMT_STREAM_TRUNCATED => axum::http::StatusCode::BAD_REQUEST,
        ec::ERR_IMAGE_PULL_FAILED => axum::http::StatusCode::BAD_GATEWAY,
        ec::ERR_SERVICE_UNAVAILABLE
        | ec::ERR_AGENT_RUNNER_UNAVAILABLE
        | ec::ERR_AGENT_CONTAINER_UNAVAILABLE
        | ec::ERR_PROXY_DISABLED
        | ec::ERR_PROXY_SERVICE_UNAVAILABLE
        | ec::ERR_RUNTIME_UNAVAILABLE
        | ec::ERR_CONTAINER_ADDRESS_NOT_READY
        | ec::ERR_DATABASE_NOT_READY
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

    #[test]
    fn error_contract_runtime_codes_have_specific_statuses() {
        use axum::http::StatusCode;
        for (code, expected) in [
            (
                "ERR_RUNTIME_CONFIGURATION",
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            ("ERR_RUNTIME_UNAVAILABLE", StatusCode::SERVICE_UNAVAILABLE),
            ("ERR_RUNTIME_TIMEOUT", StatusCode::GATEWAY_TIMEOUT),
            (
                "ERR_CONTAINER_ADDRESS_NOT_READY",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            ("ERR_DATABASE_NOT_READY", StatusCode::SERVICE_UNAVAILABLE),
            (
                "ERR_OPERATION_OUTCOME_UNKNOWN",
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            ("ERR_APP_NOT_FOUND", StatusCode::NOT_FOUND),
        ] {
            assert_eq!(super::status_from_code(code), expected, "{code}");
        }
    }

    #[tokio::test]
    async fn error_contract_response_redacts_credentials_before_output() {
        use axum::{body::to_bytes, response::IntoResponse};
        let error = AppError::with_message(
            "ERR_CONTAINER_EXEC_FAILED",
            "connect postgres://admin:private_uri_marker@localhost/app failed; POSTGRES_PASSWORD=private_env_marker; Authorization: Bearer private_token_marker",
        );
        let log_display = format!("{error}");
        let response = error.into_response();
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        for secret in [
            "private_uri_marker",
            "private_env_marker",
            "private_token_marker",
        ] {
            assert!(!body.contains(secret), "credential was exposed: {secret}");
            assert!(
                !log_display.contains(secret),
                "error logging exposed: {secret}"
            );
        }
        assert!(body.contains("localhost/app"));
    }

    #[test]
    fn http_200_conversion_preserves_error_code_operation_and_diagnostic() {
        let result: crate::HttpResult<()> = AppError::with_message(
            crate::ERR_RUNTIME_TIMEOUT,
            "Read-only status observation timed out",
        )
        .with_operation_id("operation-original".into())
        .with_error_detail(crate::ErrorDetail::new(
            crate::ERR_RUNTIME_TIMEOUT,
            "status_observation",
            "Read-only status observation timed out",
        ))
        .into_http_result("zh-CN");
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["code"], crate::ERR_RUNTIME_TIMEOUT);
        assert_eq!(value["operation_id"], "operation-original");
        assert_eq!(value["error_detail"]["stage"], "status_observation");
        assert_eq!(value["error_detail"]["retryable"], false);
        assert_eq!(value["success"], false);
    }

    #[tokio::test]
    async fn operation_in_progress_has_specific_409_data_and_preserves_original_identity() {
        use axum::{body::to_bytes, response::IntoResponse};
        let blocker = crate::UserAppOperationBlocker {
            scope: crate::UserAppOperationScope::Prod,
            operation_id: "actual-wake-holder".into(),
            kind: crate::UserAppOperationKind::Start,
            state: crate::UserAppOperationState::Running,
            step: "traffic_wake_observing".into(),
        };
        let data = crate::OperationInProgressData::from_blocker(&blocker, true, true, 20);
        let error = AppError::from_code(crate::ERR_OPERATION_IN_PROGRESS)
            .with_operation_in_progress_data(data.clone())
            .with_operation_id("accepted-parent".into())
            .with_blocker(blocker)
            .with_error_detail(
                crate::ErrorDetail::new(
                    crate::ERR_OPERATION_IN_PROGRESS,
                    "runtime_lease",
                    "Verified holder retains the application lease",
                )
                .with_retryable(true),
            );
        assert_eq!(error.status_code(), axum::http::StatusCode::CONFLICT);
        let response = error.into_response();
        assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["code"], crate::ERR_OPERATION_IN_PROGRESS);
        let mut accepted_data = data;
        accepted_data.retryable = false;
        accepted_data.retry_after_seconds = 0;
        assert_eq!(value["data"], serde_json::to_value(accepted_data).unwrap());
        assert_eq!(value["operation_id"], "accepted-parent");
        assert_eq!(value["blocker"]["operation_id"], "actual-wake-holder");
        assert_eq!(value["error_detail"]["stage"], "runtime_lease");
        assert_eq!(value["success"], false);
        assert_eq!(value["data"]["holder_kind"], "start");
        assert_eq!(value["blocker"]["kind"], "Start");
    }

    #[test]
    fn other_conflicts_cannot_acquire_new_conflict_data() {
        for code in [
            crate::ERR_CONFLICT,
            crate::ERR_APP_NOT_FOUND,
            crate::ERR_RUNTIME_UNAVAILABLE,
        ] {
            let error = AppError::with_message(code, "Original specific cause")
                .with_operation_in_progress_data(crate::OperationInProgressData::default());
            let value = serde_json::to_value(error.into_http_result::<()>("zh-CN")).unwrap();
            assert_eq!(value["code"], code);
            assert_eq!(value["message"], "Original specific cause");
            assert!(value["data"].is_null());
        }
    }

    #[test]
    fn accepted_operation_correlation_revokes_conflict_retry_in_either_builder_order() {
        let data = crate::OperationInProgressData {
            holder_operation_id: Some("different-holder".into()),
            holder_kind: Some("start".into()),
            holder_traffic_wake: true,
            holder_state: Some("running".into()),
            holder_step: Some("traffic_wake_observing".into()),
            retryable: true,
            retry_after_seconds: 20,
        };
        for error in [
            AppError::from_code(crate::ERR_OPERATION_IN_PROGRESS)
                .with_operation_in_progress_data(data.clone())
                .with_operation_id("accepted-original".into()),
            AppError::from_code(crate::ERR_OPERATION_IN_PROGRESS)
                .with_operation_id("accepted-original".into())
                .with_operation_in_progress_data(data),
        ] {
            let value = serde_json::to_value(error.into_http_result::<()>("en-US")).unwrap();
            assert_eq!(value["operation_id"], "accepted-original");
            assert_eq!(value["data"]["holder_operation_id"], "different-holder");
            assert_eq!(value["data"]["retryable"], false);
            assert_eq!(value["data"]["retry_after_seconds"], 0);
        }
    }
}
