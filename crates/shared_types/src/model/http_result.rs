use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use opentelemetry::trace::TraceContextExt;
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use utoipa::ToSchema;

use crate::error_codes::{ERR_INTERNAL_SERVER_ERROR, SUCCESS, get_error_message};
use crate::i18n::DEFAULT_LOCALE;

/// 从当前 OpenTelemetry context 获取 trace_id
fn get_trace_id_from_context() -> Option<String> {
    let span = tracing::Span::current();
    let context = span.context();
    let span_ref = context.span();
    let span_context = span_ref.span_context();

    if span_context.is_valid() {
        // 获取 trace_id 并转换为字符串
        let trace_id = span_context.trace_id();
        Some(trace_id.to_string())
    } else {
        None
    }
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, ToSchema)]
pub struct HttpResult<T> {
    /// Durable control operation associated with this response, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// In-flight operation that blocked a rejected control request. Conflict
    /// envelopes only; names the blocking scope without message parsing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<crate::UserAppOperationBlocker>,
    /// 业务状态码。`"0000"` 表示成功,其他码对应 `error_codes` 模块中常量(前缀如 `ERR_*`)。
    #[schema(example = "0000")]
    pub code: String,
    /// 人类可读消息(根据请求 `Accept-Language` 多语言化)
    #[schema(example = "success")]
    pub message: String,
    /// 业务数据。成功时为 `Some(T)`,失败时为 `None`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    /// 当前请求的 OpenTelemetry trace id,失败排查时提供给后端
    #[schema(example = "a1b2c3d4e5f6g7h8")]
    pub tid: Option<String>,
    /// 是否成功的便捷字段(由 `code == "0000"` 推导,序列化时计算)
    #[serde(skip)]
    pub success: bool,
}

impl<T> HttpResult<T> {
    pub fn with_operation_id(mut self, operation_id: String) -> Self {
        self.operation_id = Some(operation_id);
        self
    }
    pub fn with_blocker(mut self, blocker: crate::UserAppOperationBlocker) -> Self {
        self.blocker = Some(blocker);
        self
    }
    pub fn success(data: T) -> Self {
        HttpResult {
            operation_id: None,
            blocker: None,
            code: SUCCESS.to_string(),
            message: get_error_message(SUCCESS, DEFAULT_LOCALE),
            data: Some(data),
            tid: get_trace_id_from_context(),
            success: true,
        }
    }

    pub fn error(code: &str, message: &str) -> Self {
        HttpResult {
            operation_id: None,
            blocker: None,
            code: code.to_string(),
            message: message.to_string(),
            data: None,
            tid: get_trace_id_from_context(),
            success: false,
        }
    }

    /// 创建带多语言支持的错误响应
    ///
    /// # Arguments
    /// * `code` - 错误码
    /// * `locale` - 语言代码，如 "zh-CN", "en-US"
    pub fn error_with_locale(code: &str, locale: &str) -> Self {
        let message = get_error_message(code, locale);
        HttpResult {
            operation_id: None,
            blocker: None,
            code: code.to_string(),
            message,
            data: None,
            tid: get_trace_id_from_context(),
            success: false,
        }
    }

    /// 创建带多语言支持和自定义消息的错误响应
    ///
    /// # Arguments
    /// * `code` - 错误码
    /// * `_locale` - 语言代码（保留参数，用于未来扩展）
    /// * `custom_message` - 自定义错误消息（会覆盖默认翻译）
    pub fn error_with_message(code: &str, _locale: &str, custom_message: &str) -> Self {
        HttpResult {
            operation_id: None,
            blocker: None,
            code: code.to_string(),
            message: custom_message.to_string(),
            data: None,
            tid: get_trace_id_from_context(),
            success: false,
        }
    }

    /// 创建成功响应（带多语言）
    pub fn success_with_locale(data: T, locale: &str) -> Self {
        HttpResult {
            operation_id: None,
            blocker: None,
            code: SUCCESS.to_string(),
            message: get_error_message(SUCCESS, locale),
            data: Some(data),
            tid: get_trace_id_from_context(),
            success: true,
        }
    }

    pub fn internal_error(message: &str) -> Self {
        Self::error(ERR_INTERNAL_SERVER_ERROR, message)
    }

    /// 检查操作是否成功
    pub fn is_success(&self) -> bool {
        self.success
    }
}

impl<T: Serialize> Serialize for HttpResult<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct(
            "HttpResult",
            5 + usize::from(self.operation_id.is_some()) + usize::from(self.blocker.is_some()),
        )?;
        if let Some(operation_id) = &self.operation_id {
            state.serialize_field("operation_id", operation_id)?;
        }
        if let Some(blocker) = &self.blocker {
            state.serialize_field("blocker", blocker)?;
        }
        state.serialize_field("code", &self.code)?;
        state.serialize_field("message", &self.message)?;
        state.serialize_field("data", &self.data)?;
        state.serialize_field("tid", &self.tid)?;
        let is_success = self.code == "0000";
        state.serialize_field("success", &is_success)?;
        state.end()
    }
}

impl<T: Serialize> IntoResponse for HttpResult<T> {
    fn into_response(self) -> Response {
        // 创建一个新的 HttpResult，自动从 context 获取 trace_id
        let mut result = self;

        // 如果当前没有 trace_id，尝试从 OpenTelemetry context 获取
        if result.tid.is_none() {
            result.tid = get_trace_id_from_context();
        }

        match serde_json::to_string(&result) {
            Ok(body) => (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(axum::http::header::CONTENT_TYPE, "text/plain")],
                "Internal Server Error",
            )
                .into_response(),
        }
    }
}

#[cfg(test)]
mod operation_identity_tests {
    use super::HttpResult;

    #[test]
    fn optional_operation_identity_preserves_existing_response_data() {
        let legacy =
            serde_json::to_value(HttpResult::success(serde_json::json!({"app_id":"app"}))).unwrap();
        assert!(legacy.get("operation_id").is_none());
        let mut controlled = serde_json::to_value(
            HttpResult::success(serde_json::json!({"app_id":"app"}))
                .with_operation_id("operation-one".into()),
        )
        .unwrap();
        assert_eq!(controlled["operation_id"], "operation-one");
        controlled.as_object_mut().unwrap().remove("operation_id");
        assert_eq!(controlled, legacy);
        let old: HttpResult<serde_json::Value> = serde_json::from_value(legacy).unwrap();
        assert!(old.operation_id.is_none());
    }
}
