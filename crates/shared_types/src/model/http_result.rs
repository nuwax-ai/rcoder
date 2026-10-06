use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use opentelemetry::trace::TraceContextExt;
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeStruct};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use utoipa::ToSchema;

use crate::error_codes::{ERR_INTERNAL_SERVER_ERROR, SUCCESS, get_error_message};
use crate::i18n::DEFAULT_LOCALE;

/// 从当前 OpenTelemetry context 获取 trace_id
fn get_trace_id_from_context() -> Option<String> {
    current_otel_trace_id()
}

/// 当前活跃 tracing span 的 OTel trace_id（跨模块复用：代理失败出口把
/// trace_id 写进诊断日志行，实现 编号→日志→Tempo 的精确串联）。
pub fn current_otel_trace_id() -> Option<String> {
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
#[derive(Debug, ToSchema)]
pub struct HttpResult<T> {
    /// Internal typed carrier serialized as `data` only for the specific
    /// admission conflict. Ordinary success/error data remains `Option<T>`.
    #[schema(ignore)]
    pub operation_in_progress_data: Option<crate::OperationInProgressData>,
    /// Original safe diagnostic and retry evidence; absent on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<crate::ErrorDetail>,
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
    /// Business data, existing task/recovery data, or OperationInProgressData
    /// when code=ERR_OPERATION_IN_PROGRESS. Success data keeps its exact shape.
    #[schema(value_type = Option<crate::ApiBody<T>>)]
    pub data: Option<T>,
    /// 当前请求的 OpenTelemetry trace id,失败排查时提供给后端
    #[schema(example = "a1b2c3d4e5f6g7h8")]
    pub tid: Option<String>,
    /// 是否成功的便捷字段(由 `code == "0000"` 推导,序列化时计算)
    #[serde(skip)]
    pub success: bool,
}

impl<T> HttpResult<T> {
    pub fn with_operation_in_progress_data(
        mut self,
        mut data: crate::OperationInProgressData,
    ) -> Self {
        if self.code == crate::ERR_OPERATION_IN_PROGRESS {
            if self.operation_id.is_some() {
                data.retryable = false;
                data.retry_after_seconds = 0;
            }
            self.operation_in_progress_data = Some(data);
        }
        self
    }
    pub fn with_error_detail(mut self, detail: crate::ErrorDetail) -> Self {
        if self.code != SUCCESS {
            self.error_detail = Some(detail.localized(crate::current_request_locale()));
        }
        self
    }
    pub fn with_operation_id(mut self, operation_id: String) -> Self {
        self.operation_id = Some(operation_id);
        if let Some(data) = &mut self.operation_in_progress_data {
            data.retryable = false;
            data.retry_after_seconds = 0;
        }
        self
    }
    pub fn with_blocker(mut self, blocker: crate::UserAppOperationBlocker) -> Self {
        self.blocker = Some(blocker);
        self
    }
    pub fn success(data: T) -> Self {
        HttpResult {
            operation_in_progress_data: None,
            error_detail: None,
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
            operation_in_progress_data: None,
            error_detail: None,
            operation_id: None,
            blocker: None,
            code: code.to_string(),
            message: crate::sanitize_error_text(message),
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
            operation_in_progress_data: None,
            error_detail: None,
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
            operation_in_progress_data: None,
            error_detail: None,
            operation_id: None,
            blocker: None,
            code: code.to_string(),
            message: crate::sanitize_error_text(custom_message),
            data: None,
            tid: get_trace_id_from_context(),
            success: false,
        }
    }

    /// 创建成功响应（带多语言）
    pub fn success_with_locale(data: T, locale: &str) -> Self {
        HttpResult {
            operation_in_progress_data: None,
            error_detail: None,
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
            5 + usize::from(self.operation_id.is_some())
                + usize::from(self.blocker.is_some())
                + usize::from(self.code != SUCCESS && self.error_detail.is_some()),
        )?;
        if self.code != SUCCESS
            && let Some(detail) = &self.error_detail
        {
            state.serialize_field("error_detail", detail)?;
        }
        if let Some(operation_id) = &self.operation_id {
            state.serialize_field("operation_id", operation_id)?;
        }
        if let Some(blocker) = &self.blocker {
            state.serialize_field("blocker", blocker)?;
        }
        state.serialize_field("code", &self.code)?;
        state.serialize_field("message", &crate::sanitize_error_text(&self.message))?;
        if self.code == crate::ERR_OPERATION_IN_PROGRESS
            && let Some(data) = &self.operation_in_progress_data
        {
            if self.operation_id.is_some() && (data.retryable || data.retry_after_seconds != 0) {
                let mut observed = data.clone();
                observed.retryable = false;
                observed.retry_after_seconds = 0;
                state.serialize_field("data", &observed)?;
            } else {
                state.serialize_field("data", data)?;
            }
        } else {
            state.serialize_field("data", &self.data)?;
        }
        state.serialize_field("tid", &self.tid)?;
        let is_success = self.code == "0000";
        state.serialize_field("success", &is_success)?;
        state.end()
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for HttpResult<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Envelope {
            #[serde(default)]
            error_detail: Option<crate::ErrorDetail>,
            #[serde(default)]
            operation_id: Option<String>,
            #[serde(default)]
            blocker: Option<crate::UserAppOperationBlocker>,
            code: String,
            message: String,
            data: Option<serde_json::Value>,
            tid: Option<String>,
        }
        let envelope = Envelope::deserialize(deserializer)?;
        let (data, operation_in_progress_data) = match envelope.data {
            Some(value)
                if envelope.code == crate::ERR_OPERATION_IN_PROGRESS
                    && value.as_object().is_some_and(|object| {
                        [
                            "holder_operation_id",
                            "holder_kind",
                            "holder_traffic_wake",
                            "holder_state",
                            "holder_step",
                            "retryable",
                            "retry_after_seconds",
                        ]
                        .iter()
                        .all(|field| object.contains_key(*field))
                    }) =>
            {
                (
                    None,
                    Some(
                        crate::OperationInProgressData::deserialize(value)
                            .map_err(serde::de::Error::custom)?,
                    ),
                )
            }
            Some(value) => (
                Some(T::deserialize(value).map_err(serde::de::Error::custom)?),
                None,
            ),
            None => (None, None),
        };
        Ok(Self {
            operation_in_progress_data,
            error_detail: envelope.error_detail,
            operation_id: envelope.operation_id,
            blocker: envelope.blocker,
            code: envelope.code,
            message: envelope.message,
            data,
            tid: envelope.tid,
            // Preserve the existing serde(skip) contract; serialization derives
            // success from code, not a client-supplied success flag.
            success: false,
        })
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

    #[test]
    fn error_contract_roundtrip_retains_optional_diagnostic_and_task_data() {
        let input = serde_json::json!({
            "code": "ERR_DATABASE_COMMAND_FAILED",
            "message": "database execution failed",
            "data": {"task_id": "task-original", "status": "failed"},
            "tid": "trace-original",
            "operation_id": "operation-original",
            "success": false,
            "error_detail": {
                "reason_code": "database_command_failed",
                "stage": "create_database",
                "detail": "execution result was not received",
                "hint": "Inspect the original operation before retrying.",
                "retryable": false,
                "task_id": "task-original",
                "service_id": "database"
            }
        });
        let response: HttpResult<serde_json::Value> =
            serde_json::from_value(input.clone()).unwrap();
        let output = serde_json::to_value(response).unwrap();
        assert_eq!(output["error_detail"], input["error_detail"]);
        assert_eq!(output["data"], input["data"]);
        assert_eq!(output["operation_id"], input["operation_id"]);
        assert_eq!(output["tid"], input["tid"]);
    }

    #[test]
    fn successful_responses_never_emit_error_detail() {
        let result = HttpResult::success(serde_json::json!({"ready": false})).with_error_detail(
            crate::ErrorDetail::new("unexpected", "readiness", "safe cause"),
        );
        let value = serde_json::to_value(result).unwrap();
        assert!(value.get("error_detail").is_none());
        assert_eq!(value["data"]["ready"], false);
        assert_eq!(value["success"], true);
    }

    #[test]
    fn operation_in_progress_error_data_roundtrips_without_retyping_success_payload() {
        #[derive(Debug, serde::Deserialize)]
        struct Business {
            app_id: String,
        }
        let input = serde_json::json!({
            "code": crate::ERR_OPERATION_IN_PROGRESS,
            "message": "Another operation is in progress",
            "data": {
                "holder_operation_id": "actual-holder",
                "holder_kind": "restart_deployment",
                "holder_traffic_wake": false,
                "holder_state": "running",
                "holder_step": "deploy_observing",
                "retryable": true,
                "retry_after_seconds": 45
            },
            "tid": "original-trace",
            "success": false
        });
        let result: HttpResult<Business> = serde_json::from_value(input.clone()).unwrap();
        assert!(result.data.is_none());
        let detail = result.operation_in_progress_data.as_ref().unwrap();
        assert_eq!(detail.holder_operation_id.as_deref(), Some("actual-holder"));
        // The success payload can remain a concrete type that has none of the
        // conflict fields. Other error data still deserializes as that type.
        let success: HttpResult<Business> = serde_json::from_value(serde_json::json!({
            "code": "0000", "message": "success", "data": {"app_id": "original-app"},
            "tid": null, "success": true
        }))
        .unwrap();
        assert_eq!(success.data.unwrap().app_id, "original-app");
        let result: HttpResult<serde_json::Value> = serde_json::from_value(input.clone()).unwrap();
        let output = serde_json::to_value(result).unwrap();
        assert_eq!(output["data"], input["data"]);
        assert_eq!(output["tid"], input["tid"]);
    }

    #[test]
    fn specific_conflict_carrier_does_not_replace_other_success_or_failure_data() {
        for code in [
            crate::SUCCESS,
            crate::ERR_CONFLICT,
            crate::ERR_DATABASE_COMMAND_FAILED,
        ] {
            let mut result = HttpResult::error(code, "Original cause");
            result.data = Some(serde_json::json!({"task_id": "original-task"}));
            result.operation_in_progress_data = Some(crate::OperationInProgressData::default());
            let value = serde_json::to_value(result).unwrap();
            assert_eq!(value["data"]["task_id"], "original-task");
            assert!(value["data"].get("holder_operation_id").is_none());
            assert_eq!(value["success"], code == crate::SUCCESS);
        }
    }

    #[test]
    fn legacy_app_cli_in_progress_failure_data_keeps_its_existing_shape() {
        // The same string already exists in the app-cli control protocol. Only
        // the new holder object belongs to this admission-conflict carrier.
        let value = serde_json::json!({
            "code": crate::ERR_OPERATION_IN_PROGRESS,
            "message": "Original runtime control conflict",
            "data": {"task_id": "original-task", "status": "failed"},
            "tid": "original-trace", "success": false
        });
        let result: HttpResult<serde_json::Value> = serde_json::from_value(value.clone()).unwrap();
        assert!(result.operation_in_progress_data.is_none());
        let output = serde_json::to_value(result).unwrap();
        assert_eq!(output["data"], value["data"]);
    }

    #[test]
    fn accepted_identity_revokes_retry_for_both_builder_orders_and_direct_carrier_assignment() {
        let data = crate::OperationInProgressData {
            holder_operation_id: Some("actual-other-holder".into()),
            holder_kind: Some("start".into()),
            holder_traffic_wake: true,
            holder_state: Some("running".into()),
            holder_step: Some("traffic_wake_observing".into()),
            retryable: true,
            retry_after_seconds: 20,
        };
        let mut assigned =
            HttpResult::<()>::error(crate::ERR_OPERATION_IN_PROGRESS, "Original cause");
        assigned.operation_id = Some("accepted-original".into());
        assigned.operation_in_progress_data = Some(data.clone());
        for result in [
            HttpResult::<()>::error(crate::ERR_OPERATION_IN_PROGRESS, "Original cause")
                .with_operation_in_progress_data(data.clone())
                .with_operation_id("accepted-original".into()),
            HttpResult::<()>::error(crate::ERR_OPERATION_IN_PROGRESS, "Original cause")
                .with_operation_id("accepted-original".into())
                .with_operation_in_progress_data(data),
            assigned,
        ] {
            let value = serde_json::to_value(result).unwrap();
            assert_eq!(value["operation_id"], "accepted-original");
            assert_eq!(value["data"]["holder_operation_id"], "actual-other-holder");
            assert_eq!(value["data"]["retryable"], false);
            assert_eq!(value["data"]["retry_after_seconds"], 0);
        }
    }
}
