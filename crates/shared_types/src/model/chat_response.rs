use serde::{Serialize, Deserialize};

/// 聊天响应结构
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ChatResponse {
    /// 项目 ID
    #[schema(example = "test_project")]
    pub project_id: String,
    /// 会话 ID
    #[schema(example = "session456")]
    pub session_id: String,
    /// 可选的错误信息
    pub error: Option<String>,
    /// 请求ID，用于标识和追踪请求
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "req_123456789")]
    pub request_id: Option<String>,
}