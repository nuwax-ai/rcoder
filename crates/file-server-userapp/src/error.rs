//! userApp 域错误出口与响应便捷层。
//!
//! file-server 全局 `AppError` shape（`{success, code:"UNKNOWN_ERROR", error:{...}}`）
//! 服务于 TS 对齐路由不能动；userApp 域（Rust 独有业务）在此把错误统一渲染为
//! HttpResult 形态 + 语义 HTTP 状态码。翻译点在跨 crate 边界（错误从 file-server
//! 共享设施流出的 handler 出口）——`From<AppError>` 让 `?` 直接传播。

use axum::Json;
use axum::http::StatusCode;
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
        use shared_types::error_codes as ec;
        let (code, status) = match &self.0 {
            AppError::Validation(..) | AppError::ValidationI18n(..) | AppError::Business(_) => {
                (ec::ERR_VALIDATION, StatusCode::BAD_REQUEST)
            }
            AppError::Resource(_) => (ec::ERR_NOT_FOUND, StatusCode::NOT_FOUND),
            AppError::Network(_) => (ec::ERR_SERVICE_UNAVAILABLE, StatusCode::BAD_GATEWAY),
            AppError::Permission(_)
            | AppError::System(_)
            | AppError::File(_)
            | AppError::Process(_) => (
                ec::ERR_INTERNAL_SERVER_ERROR,
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        };
        let result = HttpResult::<()>::error(code, &self.0.to_string());
        (status, Json(result)).into_response()
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
