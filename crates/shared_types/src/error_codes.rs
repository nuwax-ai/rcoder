//! 错误码定义模块
//!
//! 🔴 重要原则：保持所有现有错误码不变，与前端约定保持一致

use crate::i18n::t;

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
pub const ERR_VALIDATION: &str = "ERR_VALIDATION";

/// 参数缺失或无效
pub const ERR_INVALID_PARAMS: &str = "INVALID_PARAMS";

/// 资源限制配置无效
pub const ERR_INVALID_RESOURCE_LIMITS: &str = "INVALID_RESOURCE_LIMITS";

/// 容器操作失败
pub const ERR_CONTAINER_ERROR: &str = "ERR_CONTAINER_ERROR";

/// 工作目录错误
pub const ERR_WORKSPACE_ERROR: &str = "WORKSPACE_ERROR";

/// gRPC 地址解析失败
pub const ERR_GRPC_ADDR_ERROR: &str = "GRPC_ADDR_ERROR";

/// gRPC 调用失败
pub const ERR_GRPC_ERROR: &str = "GRPC_ERROR";

/// 服务暂时不可用(如 Agent Worker 重启中)
pub const ERR_SERVICE_UNAVAILABLE: &str = "SERVICE_UNAVAILABLE";

/// Agent 内部错误(来自 agent_runner)
pub const ERR_AGENT_ERROR: &str = "AGENT_ERROR";

/// 代理服务未启用
pub const ERR_PROXY_DISABLED: &str = "PROXY_DISABLED";

/// 代理服务不可用
pub const ERR_PROXY_SERVICE_UNAVAILABLE: &str = "PROXY_SERVICE_UNAVAILABLE";

/// 未知错误
pub const ERR_UNKNOWN: &str = "UNKNOWN_ERROR";

// ========== 新增错误码（仅用于未来新功能）==========

/// 会话不存在或已完成
pub const ERR_SESSION_NOT_FOUND: &str = "ERR_SESSION_NOT_FOUND";

/// Agent 不存在或已停止
pub const ERR_AGENT_NOT_FOUND: &str = "AGENT_NOT_FOUND";

/// 容器不存在
pub const ERR_CONTAINER_NOT_FOUND: &str = "CONTAINER_NOT_FOUND";

/// HTTP 回退失败
pub const ERR_HTTP_FALLBACK_FAILED: &str = "HTTP_FALLBACK_FAILED";

/// 内部服务器错误
pub const ERR_INTERNAL_SERVER_ERROR: &str = "INTERNAL_SERVER_ERROR";

/// Resume 会话失败，已自动降级重试
pub const ERR_RESUME_FAILED: &str = "8001";

/// 降级重试次数耗尽
pub const ERR_RETRY_EXHAUSTED: &str = "8002";

/// 请求过多（DoS 防护触发）
pub const ERR_TOO_MANY_REQUESTS: &str = "TOO_MANY_REQUESTS";

/// API Key 鉴权失败
pub const ERR_API_KEY_AUTH_FAILED: &str = "4010";

/// Permission request not found or already resolved
pub const ERR_PERMISSION_NOT_FOUND: &str = "ERR_PERMISSION_NOT_FOUND";

/// Permission resolve operation failed
pub const ERR_PERMISSION_RESOLVE_FAILED: &str = "ERR_PERMISSION_RESOLVE_FAILED";

/// Permission request expired before user approval
pub const ERR_PERMISSION_EXPIRED: &str = "ERR_PERMISSION_EXPIRED";

/// 获取错误码对应的翻译 key
fn get_error_i18n_key(code: &str) -> &'static str {
    match code {
        SUCCESS => "success",
        ERR_AGENT_BUSY => "error.agent_busy",
        ERR_CANCEL_FAILED => "error.cancel_failed",
        ERR_STOP_FAILED => "error.stop_failed",
        ERR_VALIDATION => "error.validation",
        ERR_INVALID_PARAMS => "error.invalid_params",
        ERR_INVALID_RESOURCE_LIMITS => "error.invalid_resource_limits",
        ERR_CONTAINER_ERROR => "error.container_error",
        ERR_WORKSPACE_ERROR => "error.workspace_error",
        ERR_GRPC_ADDR_ERROR => "error.grpc_addr_error",
        ERR_GRPC_ERROR => "error.grpc_error",
        ERR_SERVICE_UNAVAILABLE => "error.service_unavailable",
        ERR_AGENT_ERROR => "error.agent_error",
        ERR_PROXY_DISABLED => "error.proxy_disabled",
        ERR_PROXY_SERVICE_UNAVAILABLE => "error.proxy_service_unavailable",
        ERR_SESSION_NOT_FOUND => "error.session_not_found",
        ERR_AGENT_NOT_FOUND => "error.agent_not_found",
        ERR_CONTAINER_NOT_FOUND => "error.container_not_found",
        ERR_HTTP_FALLBACK_FAILED => "error.http_fallback_failed",
        ERR_INTERNAL_SERVER_ERROR => "error.internal_server_error",
        ERR_RESUME_FAILED => "error.resume_failed",
        ERR_RETRY_EXHAUSTED => "error.retry_exhausted",
        ERR_TOO_MANY_REQUESTS => "error.too_many_requests",
        ERR_API_KEY_AUTH_FAILED => "error.api_key_auth_failed",
        ERR_PERMISSION_NOT_FOUND => "error.permission_not_found",
        ERR_PERMISSION_RESOLVE_FAILED => "error.permission_resolve_failed",
        ERR_PERMISSION_EXPIRED => "error.permission_expired",
        ERR_UNKNOWN => "error.unknown",
        _ => "error.undefined",
    }
}

/// 获取错误码的多语言描述
///
/// # Arguments
/// * `code` - 错误码
/// * `locale` - 语言代码，如 "zh-CN", "en-US"
///
/// # Returns
/// 多语言错误描述
pub fn get_error_message(code: &str, locale: &str) -> String {
    let key = get_error_i18n_key(code);
    t(key, locale)
}

/// 通过 i18n key 直接获取多语言消息
///
/// # Arguments
/// * `key` - i18n key，如 "error.user_id_required"
/// * `locale` - 语言代码
///
/// # Returns
/// 多语言消息
pub fn get_i18n_message(key: &str, locale: &str) -> String {
    t(key, locale)
}

/// 通过 i18n key 获取默认语言消息
///
/// # Arguments
/// * `key` - i18n key，如 "error.user_id_required"
///
/// # Returns
/// 默认语言的消息
pub fn get_i18n_message_default(key: &str) -> String {
    t(key, crate::i18n::DEFAULT_LOCALE)
}

/// 获取错误码的默认描述（向后兼容，使用默认语言）
///
/// 🔴 注意：此函数保留用于向后兼容，新代码请使用 `get_error_message`
pub fn get_error_description(code: &str) -> &'static str {
    match code {
        SUCCESS => "Operation successful",
        ERR_AGENT_BUSY => "Agent is executing a task",
        ERR_CANCEL_FAILED => "Cancel operation failed",
        ERR_STOP_FAILED => "Stop operation failed",
        ERR_VALIDATION => "Parameter validation failed",
        ERR_INVALID_PARAMS => "Parameter missing or invalid",
        ERR_INVALID_RESOURCE_LIMITS => "Invalid resource limit configuration",
        ERR_CONTAINER_ERROR => "Container operation failed",
        ERR_WORKSPACE_ERROR => "Workspace error",
        ERR_GRPC_ADDR_ERROR => "gRPC address resolution failed",
        ERR_GRPC_ERROR => "gRPC call failed",
        ERR_SERVICE_UNAVAILABLE => "Service temporarily unavailable",
        ERR_AGENT_ERROR => "Agent internal error",
        ERR_PROXY_DISABLED => "Proxy service not enabled",
        ERR_PROXY_SERVICE_UNAVAILABLE => "Proxy service unavailable",
        ERR_SESSION_NOT_FOUND => "Session does not exist or has completed",
        ERR_AGENT_NOT_FOUND => "Agent does not exist or has stopped",
        ERR_CONTAINER_NOT_FOUND => "Container not found",
        ERR_HTTP_FALLBACK_FAILED => "HTTP fallback failed",
        ERR_INTERNAL_SERVER_ERROR => "Internal server error",
        ERR_RESUME_FAILED => "Resume session failed",
        ERR_RETRY_EXHAUSTED => "Degraded retry count exhausted",
        ERR_TOO_MANY_REQUESTS => "Too many requests",
        ERR_API_KEY_AUTH_FAILED => "API Key authentication failed",
        ERR_PERMISSION_NOT_FOUND => "Permission request not found or already resolved",
        ERR_PERMISSION_RESOLVE_FAILED => "Permission resolve operation failed",
        ERR_PERMISSION_EXPIRED => "Permission request expired",
        ERR_UNKNOWN => "Unknown error",
        _ => "Undefined error code",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_error_codes_have_messages() {
        let codes = [
            SUCCESS,
            ERR_AGENT_BUSY,
            ERR_CANCEL_FAILED,
            ERR_STOP_FAILED,
            ERR_VALIDATION,
            ERR_INVALID_PARAMS,
            ERR_INVALID_RESOURCE_LIMITS,
            ERR_CONTAINER_ERROR,
            ERR_WORKSPACE_ERROR,
            ERR_GRPC_ADDR_ERROR,
            ERR_GRPC_ERROR,
            ERR_SERVICE_UNAVAILABLE,
            ERR_AGENT_ERROR,
            ERR_PROXY_DISABLED,
            ERR_PROXY_SERVICE_UNAVAILABLE,
            ERR_UNKNOWN,
            ERR_SESSION_NOT_FOUND,
            ERR_AGENT_NOT_FOUND,
            ERR_CONTAINER_NOT_FOUND,
            ERR_HTTP_FALLBACK_FAILED,
            ERR_INTERNAL_SERVER_ERROR,
            ERR_RESUME_FAILED,
            ERR_RETRY_EXHAUSTED,
            ERR_TOO_MANY_REQUESTS,
            ERR_API_KEY_AUTH_FAILED,
            ERR_PERMISSION_NOT_FOUND,
            ERR_PERMISSION_RESOLVE_FAILED,
            ERR_PERMISSION_EXPIRED,
        ];

        for code in codes {
            assert!(
                !get_error_message(code, "en-US").is_empty(),
                "missing en-US: {code}"
            );
            assert!(
                !get_error_message(code, "zh-CN").is_empty(),
                "missing zh-CN: {code}"
            );
            assert!(
                !get_error_message(code, "zh-TW").is_empty(),
                "missing zh-TW: {code}"
            );
        }
    }
}
