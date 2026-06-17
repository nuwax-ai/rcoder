//! gRPC 错误分类和处理
//!
//! 使用枚举统一包装不同类型的 gRPC 错误，避免 downcast_ref 类型转换。

use tonic::{Code, Status, transport};

/// gRPC 错误分类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrpcErrorCategory {
    /// 可重试错误（网络问题、资源不足等临时性错误）
    Retryable,
    /// 不可重试错误（参数错误、权限问题等客户端错误）
    NonRetryable,
    /// 永久性错误（未找到、未实现等服务端永久性问题）
    Permanent,
}

/// 统一的 gRPC 错误类型
///
/// 在错误发生时就进行分类，避免下游使用 downcast_ref。
/// 这种设计更符合 Rust 的类型系统，错误信息不会丢失。
#[derive(Debug)]
pub enum GrpcError {
    /// gRPC 业务层错误（Status）
    Status(Status),
    /// gRPC 连接层错误（transport::Error）
    ///
    /// 包括：KeepAliveTimedOut, ConnectionRefused, ConnectionReset 等
    /// 这类错误通常意味着连接已失效，应该重试
    Transport(transport::Error),
}

impl GrpcError {
    /// 判断错误是否应该重试
    pub fn should_retry(&self) -> bool {
        match self {
            GrpcError::Status(status) => categorize_grpc_error(status) == GrpcErrorCategory::Retryable,
            // transport::Error 表示连接层错误，通常应该重试
            GrpcError::Transport(_) => true,
        }
    }

    /// 获取错误分类
    pub fn category(&self) -> GrpcErrorCategory {
        match self {
            GrpcError::Status(status) => categorize_grpc_error(status),
            GrpcError::Transport(_) => GrpcErrorCategory::Retryable,
        }
    }
}

impl std::fmt::Display for GrpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GrpcError::Status(status) => write!(f, "gRPC Status: {} ({})", status.message(), status.code()),
            GrpcError::Transport(err) => write!(f, "gRPC Transport: {}", err),
        }
    }
}

impl std::error::Error for GrpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GrpcError::Status(status) => Some(status),
            GrpcError::Transport(err) => Some(err),
        }
    }
}

// 从 tonic::Status 转换
impl From<Status> for GrpcError {
    fn from(status: Status) -> Self {
        GrpcError::Status(status)
    }
}

// 从 tonic::transport::Error 转换
impl From<transport::Error> for GrpcError {
    fn from(err: transport::Error) -> Self {
        GrpcError::Transport(err)
    }
}

// GrpcError 已经实现了 std::error::Error，所以可以自动转换为 anyhow::Error
// 无需显式实现 From<GrpcError> for anyhow::Error

/// 基于 Tonic Status Code 分类 gRPC 错误
///
/// 根据 gRPC 标准错误码判断错误是否应该重试
pub fn categorize_grpc_error(status: &Status) -> GrpcErrorCategory {
    match status.code() {
        // ✅ 可重试错误：网络问题、资源不足、瞬时故障
        Code::Unavailable |       // 服务不可用（最常见的网络问题）
        Code::DeadlineExceeded |  // 超时（可能是临时性网络延迟）
        Code::ResourceExhausted | // 资源耗尽（服务器过载，可能恢复）
        Code::Aborted |           // 操作被中止（可能是并发冲突，重试可能成功）
        Code::Internal |          // 内部错误（可能是临时性服务器问题）
        Code::Unknown =>          // 未知错误（保守策略：允许重试）
            GrpcErrorCategory::Retryable,

        // ❌ 永久性错误：服务端不支持或资源不存在
        Code::NotFound |          // 未找到资源
        Code::Unimplemented |     // 方法未实现
        Code::OutOfRange =>       // 超出范围（通常是客户端逻辑错误）
            GrpcErrorCategory::Permanent,

        // ❌ 不可重试错误：客户端问题，重试也不会成功
        Code::InvalidArgument |   // 参数错误
        Code::Unauthenticated |   // 未认证
        Code::PermissionDenied |  // 权限不足
        Code::FailedPrecondition | // 前置条件失败
        Code::AlreadyExists |     // 资源已存在
        Code::Cancelled =>        // 用户取消（不应重试）
            GrpcErrorCategory::NonRetryable,

        // ✅ OK - 理论上不应该走到这里
        Code::Ok => GrpcErrorCategory::NonRetryable,

        // ⚠️ DataLoss - 严重错误，但可能是临时性的
        Code::DataLoss => GrpcErrorCategory::Retryable,
    }
}

/// 获取错误的友好描述
pub fn get_error_description(status: &Status) -> &'static str {
    match status.code() {
        Code::Ok => "Success",
        Code::Cancelled => "Operation cancelled",
        Code::Unknown => "Unknown error",
        Code::InvalidArgument => "Invalid argument",
        Code::DeadlineExceeded => "Request timeout",
        Code::NotFound => "Resource not found",
        Code::AlreadyExists => "Resource already exists",
        Code::PermissionDenied => "Permission denied",
        Code::ResourceExhausted => "Resource exhausted",
        Code::FailedPrecondition => "Precondition failed",
        Code::Aborted => "Operation aborted",
        Code::OutOfRange => "Out of range",
        Code::Unimplemented => "Method not implemented",
        Code::Internal => "Internal server error",
        Code::Unavailable => "Service unavailable",
        Code::DataLoss => "Data loss",
        Code::Unauthenticated => "Unauthenticated",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_categorize_retryable_errors() {
        let retryable_codes = vec![
            Code::Unavailable,
            Code::DeadlineExceeded,
            Code::ResourceExhausted,
            Code::Aborted,
            Code::Internal,
            Code::Unknown,
        ];

        for code in retryable_codes {
            let status = Status::new(code, "test error");
            assert_eq!(
                categorize_grpc_error(&status),
                GrpcErrorCategory::Retryable,
                "Code {:?} should be retryable",
                code
            );
        }
    }

    #[test]
    fn test_categorize_non_retryable_errors() {
        let non_retryable_codes = vec![
            Code::InvalidArgument,
            Code::Unauthenticated,
            Code::PermissionDenied,
            Code::FailedPrecondition,
            Code::AlreadyExists,
            Code::Cancelled,
        ];

        for code in non_retryable_codes {
            let status = Status::new(code, "test error");
            assert_eq!(
                categorize_grpc_error(&status),
                GrpcErrorCategory::NonRetryable,
                "Code {:?} should not be retryable",
                code
            );
        }
    }

    #[test]
    fn test_categorize_permanent_errors() {
        let permanent_codes = vec![Code::NotFound, Code::Unimplemented, Code::OutOfRange];

        for code in permanent_codes {
            let status = Status::new(code, "test error");
            assert_eq!(
                categorize_grpc_error(&status),
                GrpcErrorCategory::Permanent,
                "Code {:?} should be permanent",
                code
            );
        }
    }

    #[test]
    fn test_grpc_error_should_retry() {
        // 测试 Status 类型的可重试错误
        let retryable_status = GrpcError::Status(Status::unavailable("service unavailable"));
        assert!(retryable_status.should_retry());

        // 测试 Status 类型的不可重试错误
        let non_retryable_status = GrpcError::Status(Status::invalid_argument("bad request"));
        assert!(!non_retryable_status.should_retry());

        // 注意：tonic::transport::Error 无法直接构造，其 should_retry() 始终返回 true
        // 这个逻辑在 GrpcError::should_retry() 中已经实现
    }

    #[test]
    fn test_grpc_error_category() {
        // 测试 Status 错误分类
        let unavailable = GrpcError::Status(Status::unavailable("unavailable"));
        assert_eq!(unavailable.category(), GrpcErrorCategory::Retryable);

        let invalid_arg = GrpcError::Status(Status::invalid_argument("invalid"));
        assert_eq!(invalid_arg.category(), GrpcErrorCategory::NonRetryable);

        let not_found = GrpcError::Status(Status::not_found("not found"));
        assert_eq!(not_found.category(), GrpcErrorCategory::Permanent);

        // 注意：tonic::transport::Error 无法直接构造，其 category() 始终返回 Retryable
    }

    #[test]
    fn test_grpc_error_display() {
        let status_err = GrpcError::Status(Status::unavailable("service down"));
        let display = format!("{}", status_err);
        // Display 格式是 "gRPC Status: {message} ({code_description})"
        assert!(display.contains("service down"), "Display should contain message: {}", display);
        assert!(display.contains("currently unavailable"), "Display should contain code description: {}", display);
    }

    #[test]
    fn test_grpc_error_from_conversions() {
        // 测试 From<Status> 转换
        let status = Status::unavailable("test");
        let err: GrpcError = status.into();
        assert!(err.should_retry());

        // 注意：tonic::transport::Error 无法直接构造，From 转换在实际运行时自动完成
    }
}
