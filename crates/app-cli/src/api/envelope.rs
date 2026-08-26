//! 管理 API 统一响应信封（app-cli 本地版）。
//!
//! wire 形态与主 workspace `shared_types::HttpResult` 逐字段对齐：
//! `{code, message, data, tid, success}` 恒 5 键（失败 `data` 恒 `null`；
//! `success` 由 `code == "0000"` 推导）。app-cli 是独立 workspace（root exclude，
//! 锁解耦设计，无 shared_types 依赖），故本地复制——**改字段必须两处同步**，
//! 本文件 mod tests 的 wire 锁快照负责锚定漂移。
//!
//! 与 shared_types 版的两处有意差异：
//! - `tid` 恒 `null`（app-cli 无 OTel 设施；消费方按可空绑定即兼容）；
//! - 组装时保留语义 HTTP 状态码（shared_types IntoResponse 恒 200；app-cli 的
//!   调用方——kubelet 探针、rcoder 热部署客户端——依赖 202/409/400/503）。
//!
//! 豁免信封的端点：`/health`、`/ready`（kubelet 探针只看状态码）、
//! `/v1/logs/stream`（SSE 事件流）、`/v1/proxy/effective-config`（TOML 文本直读）。

use axum::Json;
use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// 成功业务码（对齐 shared_types::error_codes::SUCCESS）。
const SUCCESS_CODE: &str = "0000";
/// 成功 message。shared_types 走 i18n（缺省英文文案）；app-cli 无 i18n，
/// 固定 "success"——消费方不应依赖该文案，只看 code/success/data。
const SUCCESS_MESSAGE: &str = "success";

/// 管理 API 统一响应信封。
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct HttpResult<T> {
    /// 业务状态码（`"0000"` = 成功；失败为端点特定错误码，如 `INVALID_LOG_QUERY`）
    pub code: String,
    /// 人类可读消息
    pub message: String,
    /// 业务数据（失败恒 `null`）
    pub data: Option<T>,
    /// trace id（app-cli 无 OTel，恒 `null`）
    pub tid: Option<String>,
    /// 是否成功（`code == "0000"`）
    pub success: bool,
}

impl<T> HttpResult<T> {
    fn success(data: T) -> Self {
        Self {
            code: SUCCESS_CODE.to_string(),
            message: SUCCESS_MESSAGE.to_string(),
            data: Some(data),
            tid: None,
            success: true,
        }
    }

    fn error(code: &str, message: String) -> Self {
        Self {
            code: code.to_string(),
            message,
            data: None,
            tid: None,
            success: false,
        }
    }
}

/// 组装成功信封响应（data 载荷 + 指定状态码）。
pub(super) fn ok<T: serde::Serialize>(status: StatusCode, data: T) -> Response {
    (status, Json(HttpResult::success(data))).into_response()
}

/// 组装失败信封响应（data=null + 错误码/消息 + 指定状态码）。
pub(super) fn error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (status, Json(HttpResult::<()>::error(code, message.into()))).into_response()
}

/// JSON 请求提取器：解析拒绝（格式错/类型错）也回信封错误（400 INVALID_BODY），
/// 不漏 axum 默认的纯文本 400。
pub(crate) struct ApiJson<T>(pub(crate) T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    axum::Json<T>: FromRequest<S, Rejection = axum::extract::rejection::JsonRejection>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(error(
                StatusCode::BAD_REQUEST,
                "INVALID_BODY",
                format!("invalid JSON request: {rejection}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// wire 锁：成功信封与 shared_types::HttpResult 逐键一致（恒 5 键，data 载荷透出）。
    #[test]
    fn success_envelope_wire_is_locked() {
        let value = serde_json::to_value(HttpResult::success(vec!["src-a"])).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "code": "0000",
                "message": "success",
                "data": ["src-a"],
                "tid": null,
                "success": true,
            })
        );
    }

    /// wire 锁：失败信封 data 恒 null、success=false。
    #[test]
    fn error_envelope_wire_is_locked() {
        let value = serde_json::to_value(HttpResult::<()>::error(
            "INVALID_LOG_QUERY",
            "bad selector".into(),
        ))
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "code": "INVALID_LOG_QUERY",
                "message": "bad selector",
                "data": null,
                "tid": null,
                "success": false,
            })
        );
    }
}
