//! 错误码定义模块
//!
//! 🔴 重要原则：保持所有现有错误码不变，与前端约定保持一致

// ========== 成功状态码 ==========
/// 操作成功
pub const SUCCESS: &str = "0000";

// ========== 现有业务错误码（保持不变，与前端约定一致）==========

/// Agent 正在执行任务，禁止并发请求（与前端约定的错误码）
pub const ERR_AGENT_BUSY: &str = "9010";

/// 取消操作失败（保持现有格式）
pub const ERR_CANCEL_FAILED: &str = "CANCEL001";

/// 停止操作失败（保持现有格式）
pub const ERR_STOP_FAILED: &str = "STOP001";

// ========== 现有错误码（字符串格式，保持不变）==========

/// 参数验证失败
pub const ERR_VALIDATION: &str = "VALIDATION_ERROR";

/// 参数缺失或无效
pub const ERR_INVALID_PARAMS: &str = "INVALID_PARAMS";

/// 资源限制配置无效
pub const ERR_INVALID_RESOURCE_LIMITS: &str = "INVALID_RESOURCE_LIMITS";

/// 容器操作失败
pub const ERR_CONTAINER_ERROR: &str = "CONTAINER_ERROR";

/// 工作目录错误
pub const ERR_WORKSPACE_ERROR: &str = "WORKSPACE_ERROR";

/// gRPC 地址解析失败
pub const ERR_GRPC_ADDR_ERROR: &str = "GRPC_ADDR_ERROR";

/// gRPC 调用失败
pub const ERR_GRPC_ERROR: &str = "GRPC_ERROR";

/// Agent 内部错误（来自 agent_runner）
pub const ERR_AGENT_ERROR: &str = "AGENT_ERROR";

/// 代理服务未启用
pub const ERR_PROXY_DISABLED: &str = "PROXY_DISABLED";

/// 代理服务不可用
pub const ERR_PROXY_SERVICE_UNAVAILABLE: &str = "PROXY_SERVICE_UNAVAILABLE";

/// 未知错误
pub const ERR_UNKNOWN: &str = "UNKNOWN_ERROR";

// ========== 新增错误码（仅用于未来新功能）==========

/// 会话不存在或已完成
pub const ERR_SESSION_NOT_FOUND: &str = "SESSION_NOT_FOUND";

/// Agent 不存在或已停止
pub const ERR_AGENT_NOT_FOUND: &str = "AGENT_NOT_FOUND";

/// HTTP 回退失败
pub const ERR_HTTP_FALLBACK_FAILED: &str = "HTTP_FALLBACK_FAILED";

/// 内部服务器错误
pub const ERR_INTERNAL_SERVER_ERROR: &str = "INTERNAL_SERVER_ERROR";

/// 获取错误码的默认描述
pub fn get_error_description(code: &str) -> &'static str {
    match code {
        SUCCESS => "操作成功",
        ERR_AGENT_BUSY => "Agent 正在执行任务",
        ERR_CANCEL_FAILED => "取消操作失败",
        ERR_STOP_FAILED => "停止操作失败",
        ERR_VALIDATION => "参数验证失败",
        ERR_INVALID_PARAMS => "参数缺失或无效",
        ERR_INVALID_RESOURCE_LIMITS => "资源限制配置无效",
        ERR_CONTAINER_ERROR => "容器操作失败",
        ERR_WORKSPACE_ERROR => "工作目录错误",
        ERR_GRPC_ADDR_ERROR => "gRPC 地址解析失败",
        ERR_GRPC_ERROR => "gRPC 调用失败",
        ERR_AGENT_ERROR => "Agent 内部错误",
        ERR_PROXY_DISABLED => "代理服务未启用",
        ERR_PROXY_SERVICE_UNAVAILABLE => "代理服务不可用",
        ERR_SESSION_NOT_FOUND => "会话不存在或已完成",
        ERR_AGENT_NOT_FOUND => "Agent 不存在或已停止",
        ERR_HTTP_FALLBACK_FAILED => "HTTP 回退失败",
        ERR_INTERNAL_SERVER_ERROR => "内部服务器错误",
        ERR_UNKNOWN => "未知错误",
        _ => "未定义的错误码",
    }
}
