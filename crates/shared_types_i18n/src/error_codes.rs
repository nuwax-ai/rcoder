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

// ========== Agent Management API 错误码 (P0-1) ==========

/// Agent 未找到
pub const ERR_AGENT_MGMT_NOT_FOUND: &str = "ERR_AGENT_MGMT_NOT_FOUND";

/// Agent 已安装（无法重复安装）
pub const ERR_AGENT_MGMT_ALREADY_INSTALLED: &str = "ERR_AGENT_MGMT_ALREADY_INSTALLED";

/// Agent manifest 格式无效
pub const ERR_AGENT_MGMT_INVALID_MANIFEST: &str = "ERR_AGENT_MGMT_INVALID_MANIFEST";

/// 下载文件 checksum 与指定值不一致
pub const ERR_AGENT_MGMT_CHECKSUM_MISMATCH: &str = "ERR_AGENT_MGMT_CHECKSUM_MISMATCH";

/// 压缩包解压后大小超限(zip bomb 防护)
pub const ERR_AGENT_MGMT_ARCHIVE_BOMB: &str = "ERR_AGENT_MGMT_ARCHIVE_BOMB";

/// 压缩包包含路径遍历条目(安全防护)
pub const ERR_AGENT_MGMT_PATH_TRAVERSAL: &str = "ERR_AGENT_MGMT_PATH_TRAVERSAL";

/// 执行命令超时
pub const ERR_AGENT_MGMT_COMMAND_TIMEOUT: &str = "ERR_AGENT_MGMT_COMMAND_TIMEOUT";

/// 安装过程失败
pub const ERR_AGENT_MGMT_INSTALL_FAILED: &str = "ERR_AGENT_MGMT_INSTALL_FAILED";

/// 卸载过程失败
pub const ERR_AGENT_MGMT_UNINSTALL_FAILED: &str = "ERR_AGENT_MGMT_UNINSTALL_FAILED";

/// 健康检查失败
pub const ERR_AGENT_MGMT_CHECK_FAILED: &str = "ERR_AGENT_MGMT_CHECK_FAILED";

/// 二进制文件超过大小上限
pub const ERR_AGENT_MGMT_BINARY_TOO_LARGE: &str = "ERR_AGENT_MGMT_BINARY_TOO_LARGE";

/// 不支持的安装类型
pub const ERR_AGENT_MGMT_UNSUPPORTED_TYPE: &str = "ERR_AGENT_MGMT_UNSUPPORTED_TYPE";

/// 内置 agent 受保护,不可卸载
pub const ERR_AGENT_MGMT_BUILTIN_PROTECTED: &str = "ERR_AGENT_MGMT_BUILTIN_PROTECTED";

/// 流式上传在传输中途断开
pub const ERR_AGENT_MGMT_STREAM_TRUNCATED: &str = "ERR_AGENT_MGMT_STREAM_TRUNCATED";

/// 磁盘空间不足
pub const ERR_AGENT_MGMT_DISK_FULL: &str = "ERR_AGENT_MGMT_DISK_FULL";

/// 权限不足(无法写入安装目录)
pub const ERR_AGENT_MGMT_PERMISSION_DENIED: &str = "ERR_AGENT_MGMT_PERMISSION_DENIED";

/// 未知 agent_id
pub const ERR_AGENT_MGMT_UNKNOWN_AGENT: &str = "ERR_AGENT_MGMT_UNKNOWN_AGENT";

/// 上传 chunk 格式无效
pub const ERR_AGENT_MGMT_INVALID_CHUNK: &str = "ERR_AGENT_MGMT_INVALID_CHUNK";

/// platforms 中无匹配当前系统的 URL
pub const ERR_AGENT_MGMT_PLATFORM_NOT_FOUND: &str = "ERR_AGENT_MGMT_PLATFORM_NOT_FOUND";

/// version 格式不合法(非语义化版本号)
pub const ERR_AGENT_MGMT_INVALID_VERSION: &str = "ERR_AGENT_MGMT_INVALID_VERSION";

/// 项目不存在(P0-4: rcoder 转发层用于找不到 project_id 时)
pub const ERR_PROJECT_NOT_FOUND: &str = "ERR_PROJECT_NOT_FOUND";

/// Agent Runner 容器不可用(P0-4: gRPC 调用失败 / 容器离线)
pub const ERR_AGENT_RUNNER_UNAVAILABLE: &str = "ERR_AGENT_RUNNER_UNAVAILABLE";

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
        ERR_AGENT_MGMT_NOT_FOUND => "error.agent_mgmt_not_found",
        ERR_AGENT_MGMT_ALREADY_INSTALLED => "error.agent_mgmt_already_installed",
        ERR_AGENT_MGMT_INVALID_MANIFEST => "error.agent_mgmt_invalid_manifest",
        ERR_AGENT_MGMT_CHECKSUM_MISMATCH => "error.agent_mgmt_checksum_mismatch",
        ERR_AGENT_MGMT_ARCHIVE_BOMB => "error.agent_mgmt_archive_bomb",
        ERR_AGENT_MGMT_PATH_TRAVERSAL => "error.agent_mgmt_path_traversal",
        ERR_AGENT_MGMT_COMMAND_TIMEOUT => "error.agent_mgmt_command_timeout",
        ERR_AGENT_MGMT_INSTALL_FAILED => "error.agent_mgmt_install_failed",
        ERR_AGENT_MGMT_UNINSTALL_FAILED => "error.agent_mgmt_uninstall_failed",
        ERR_AGENT_MGMT_CHECK_FAILED => "error.agent_mgmt_check_failed",
        ERR_AGENT_MGMT_BINARY_TOO_LARGE => "error.agent_mgmt_binary_too_large",
        ERR_AGENT_MGMT_UNSUPPORTED_TYPE => "error.agent_mgmt_unsupported_type",
        ERR_AGENT_MGMT_BUILTIN_PROTECTED => "error.agent_mgmt_builtin_protected",
        ERR_AGENT_MGMT_STREAM_TRUNCATED => "error.agent_mgmt_stream_truncated",
        ERR_AGENT_MGMT_DISK_FULL => "error.agent_mgmt_disk_full",
        ERR_AGENT_MGMT_PERMISSION_DENIED => "error.agent_mgmt_permission_denied",
        ERR_AGENT_MGMT_UNKNOWN_AGENT => "error.agent_mgmt_unknown_agent",
        ERR_AGENT_MGMT_INVALID_CHUNK => "error.agent_mgmt_invalid_chunk",
        ERR_AGENT_MGMT_PLATFORM_NOT_FOUND => "error.agent_mgmt_platform_not_found",
        ERR_AGENT_MGMT_INVALID_VERSION => "error.agent_mgmt_invalid_version",
        ERR_PROJECT_NOT_FOUND => "error.project_not_found",
        ERR_AGENT_RUNNER_UNAVAILABLE => "error.agent_runner_unavailable",
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
        ERR_AGENT_MGMT_NOT_FOUND => "Agent not found",
        ERR_AGENT_MGMT_ALREADY_INSTALLED => "Agent already installed",
        ERR_AGENT_MGMT_INVALID_MANIFEST => "Invalid agent manifest",
        ERR_AGENT_MGMT_CHECKSUM_MISMATCH => "Checksum mismatch",
        ERR_AGENT_MGMT_ARCHIVE_BOMB => "Archive too large (possible zip bomb)",
        ERR_AGENT_MGMT_PATH_TRAVERSAL => "Archive contains path traversal entries",
        ERR_AGENT_MGMT_COMMAND_TIMEOUT => "Command execution timeout",
        ERR_AGENT_MGMT_INSTALL_FAILED => "Agent installation failed",
        ERR_AGENT_MGMT_UNINSTALL_FAILED => "Agent uninstallation failed",
        ERR_AGENT_MGMT_CHECK_FAILED => "Agent health check failed",
        ERR_AGENT_MGMT_BINARY_TOO_LARGE => "Binary file exceeds size limit",
        ERR_AGENT_MGMT_UNSUPPORTED_TYPE => "Unsupported install type",
        ERR_AGENT_MGMT_BUILTIN_PROTECTED => "Builtin agent is protected from uninstallation",
        ERR_AGENT_MGMT_STREAM_TRUNCATED => "Upload stream truncated",
        ERR_AGENT_MGMT_DISK_FULL => "Insufficient disk space",
        ERR_AGENT_MGMT_PERMISSION_DENIED => "Permission denied",
        ERR_AGENT_MGMT_UNKNOWN_AGENT => "Unknown agent_id",
        ERR_AGENT_MGMT_INVALID_CHUNK => "Invalid upload chunk",
        ERR_PROJECT_NOT_FOUND => "Project not found or stopped",
        ERR_AGENT_RUNNER_UNAVAILABLE => "Agent Runner container is unavailable",
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
            ERR_AGENT_MGMT_NOT_FOUND,
            ERR_AGENT_MGMT_ALREADY_INSTALLED,
            ERR_AGENT_MGMT_INVALID_MANIFEST,
            ERR_AGENT_MGMT_CHECKSUM_MISMATCH,
            ERR_AGENT_MGMT_ARCHIVE_BOMB,
            ERR_AGENT_MGMT_PATH_TRAVERSAL,
            ERR_AGENT_MGMT_COMMAND_TIMEOUT,
            ERR_AGENT_MGMT_INSTALL_FAILED,
            ERR_AGENT_MGMT_UNINSTALL_FAILED,
            ERR_AGENT_MGMT_CHECK_FAILED,
            ERR_AGENT_MGMT_BINARY_TOO_LARGE,
            ERR_AGENT_MGMT_UNSUPPORTED_TYPE,
            ERR_AGENT_MGMT_BUILTIN_PROTECTED,
            ERR_AGENT_MGMT_STREAM_TRUNCATED,
            ERR_AGENT_MGMT_DISK_FULL,
            ERR_AGENT_MGMT_PERMISSION_DENIED,
            ERR_AGENT_MGMT_UNKNOWN_AGENT,
            ERR_AGENT_MGMT_INVALID_CHUNK,
            ERR_PROJECT_NOT_FOUND,
            ERR_AGENT_RUNNER_UNAVAILABLE,
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
