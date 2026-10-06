//! userApp 域错误出口与响应便捷层。
//!
//! file-server 的既有 `AppError` 包装服务于 TS 对齐路由；新增共享运行时诊断
//! 保留真实 code。userApp 域（Rust 独有业务）在此把错误统一渲染为
//! HttpResult 形态 + HTTP 200。翻译点在跨 crate 边界（错误从 file-server
//! 共享设施流出的 handler 出口）——`From<AppError>` 让 `?` 直接传播。

use axum::Json;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use shared_types::HttpResult;

use file_server::error::AppError;

/// userApp handler 的 Err 侧类型：`Result<Json<HttpResult<T>>, UserAppError>`。
pub struct UserAppError(AppError);

/// 跨 crate 边界翻译：file-server 共享设施（computer impl / DevServerManager /
/// read_dev_log 等）的 AppError 流入 userApp 域即转为本类型（`?` 可直接传播）。
impl From<AppError> for UserAppError {
    fn from(e: AppError) -> Self {
        Self(e)
    }
}

impl IntoResponse for UserAppError {
    fn into_response(self) -> Response {
        if let AppError::RuntimeDiagnostic(origin) = self.0 {
            return Json(
                origin
                    .into_http_result::<serde_json::Value>(shared_types::current_request_locale()),
            )
            .into_response();
        }
        let code = app_error_code(&self.0);
        let mut result = HttpResult::<serde_json::Value>::error(code, &self.0.to_string());
        if let AppError::RuntimeRecovery(_, details) = &self.0 {
            result.data = Some(details.clone());
        }
        Json(result).into_response()
    }
}

pub(crate) fn app_error_code(error: &AppError) -> &str {
    use shared_types::error_codes as ec;
    match error {
        AppError::RuntimeDiagnostic(origin) => match origin.as_ref() {
            shared_types::AppError::Structured(detail) => &detail.code,
            _ => ec::ERR_INTERNAL_SERVER_ERROR,
        },
        AppError::Validation(..) | AppError::ValidationI18n(..) | AppError::Business(_) => {
            ec::ERR_VALIDATION
        }
        AppError::Conflict(_) | AppError::RuntimeRecovery(..) => ec::ERR_CONFLICT,
        AppError::Resource(_) => ec::ERR_NOT_FOUND,
        AppError::Network(_) => ec::ERR_SERVICE_UNAVAILABLE,
        AppError::Permission(_)
        | AppError::System(_)
        | AppError::CommandExecution { .. }
        | AppError::File(_)
        | AppError::Process(_)
        | AppError::ProcessPortInUse { .. } => ec::ERR_INTERNAL_SERVER_ERROR,
    }
}

/// Ok 侧便捷包装：`Ok(success_reply(data))` → 200 + HttpResult 成功信封。
pub fn success_reply<T: Serialize>(data: T) -> Json<HttpResult<T>> {
    Json(HttpResult::success(data))
}

/// `AppResult<T>` 一行转 handler 返回类型（Ok 侧包信封 / Err 侧跨边界翻译）。
/// 迁移自 file-server userapp 域的 `reply()`，调用点形态不变。
pub fn reply<T: Serialize>(
    r: file_server::error::AppResult<T>,
) -> Result<Json<HttpResult<T>>, UserAppError> {
    match r {
        Ok(data) => Ok(success_reply(data)),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[tokio::test]
    async fn management_recovery_preserves_identity_and_reason_in_userapp_envelope() {
        let details = serde_json::json!({
            "supervisor_id": "original-owner", "generation": "original-generation",
            "operation_id": "original-stop", "phase": "recovery_required",
            "problem": {"code": "cleanup_unconfirmed", "message": "physical exit unconfirmed"}
        });
        let response = UserAppError::from(AppError::RuntimeRecovery(
            "management recovery required".into(),
            details.clone(),
        ))
        .into_response();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "preserve UserApp envelope contract"
        );
        let body = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], shared_types::error_codes::ERR_CONFLICT);
        assert_eq!(body["success"], false);
        assert_eq!(body["data"], details);
    }

    #[tokio::test]
    async fn workspace_runtime_diagnostic_keeps_userapp_200_and_original_carrier() {
        let origin = shared_types::AppError::with_message(
            shared_types::error_codes::ERR_OPERATION_OUTCOME_UNKNOWN,
            "workspace_ensure: reply lost password=hidden-marker",
        )
        .with_operation_id("original-workspace-operation".into())
        .with_error_detail(
            shared_types::ErrorDetail::new(
                shared_types::error_codes::ERR_RUNTIME_UNAVAILABLE,
                "workspace_ensure",
                "reply lost password=hidden-marker",
            )
            .with_task_id("retained-original-task"),
        );
        let response = shared_types::scope_request_locale("zh-CN", async {
            UserAppError::from(AppError::runtime_diagnostic(origin)).into_response()
        })
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["success"], false);
        assert_eq!(
            body["code"],
            shared_types::error_codes::ERR_OPERATION_OUTCOME_UNKNOWN
        );
        assert_eq!(body["operation_id"], "original-workspace-operation");
        assert_eq!(body["error_detail"]["task_id"], "retained-original-task");
        assert_eq!(body["error_detail"]["stage"], "workspace_ensure");
        assert_eq!(body["error_detail"]["retryable"], false);
        assert_eq!(
            body["error_detail"]["hint"],
            shared_types::error_codes::get_error_hint(
                shared_types::error_codes::ERR_RUNTIME_UNAVAILABLE,
                "zh-CN"
            )
        );
        assert!(!body.to_string().contains("hidden-marker"));
        assert!(
            body.get("error").is_none(),
            "preserve UserApp HttpResult shape"
        );
    }
}
