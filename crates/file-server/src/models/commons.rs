//! 全 crate 公共响应信封与 OpenAPI 二进制占位。

use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;

/// OpenAPI multipart binary item，支持单文件和文件数组。
#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(ToSchema)]
#[schema(value_type = String, format = Binary)]
pub struct BinaryFile(String);

/// JSON 成功响应的公共字段。具体接口会附加各自业务字段。
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SuccessResponse {
    pub success: bool,
    pub message: Option<String>,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
#[schema(as = FileServerErrorBody)]
pub struct ErrorDetail {
    /// 既有文件服务错误类型。
    pub r#type: String,
    /// 按请求语言展示的具体错误原因。
    pub message: String,
    /// CST 格式的响应时间。
    pub timestamp: String,
    /// 当前文件请求身份，与响应 x-request-id 一致。
    pub request_id: String,
    /// 既有变体的附加恢复或校验信息。
    #[schema(value_type = Object)]
    pub details: Option<Value>,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorResponse {
    /// 错误响应固定为 false。
    pub success: bool,
    /// 旧变体使用 UNKNOWN_ERROR；运行时诊断保留共享 ERR_* 原因。
    pub code: String,
    /// 既有文件服务错误包装。
    pub error: ErrorDetail,
    /// 原失败阶段与安全重试证据；普通旧错误不含该字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<shared_types::ErrorDetail>,
    /// 运行时返回的真实操作身份；未知时不生成。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// 已确认的阻塞操作信息。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocker: Option<shared_types::UserAppOperationBlocker>,
    /// 当前真实 OpenTelemetry trace 身份。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tid: Option<String>,
}
