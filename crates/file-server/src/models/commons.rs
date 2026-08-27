//! 全 crate 公共响应信封与 OpenAPI 二进制占位（原 openapi.rs 内联定义）。

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
pub struct ErrorDetail {
    pub r#type: String,
    pub message: String,
    pub timestamp: String,
    pub request_id: String,
    #[schema(value_type = Object)]
    pub details: Option<Value>,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorResponse {
    pub success: bool,
    pub code: String,
    pub error: ErrorDetail,
}
