//! gRPC 错误分类和处理
//!
//! 基于 Tonic 的 Status Code 进行智能错误分类，优化重试策略

use tonic::{Code, Status};

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

/// 基于 Tonic Status Code 分类 gRPC 错误
///
/// 根据 gRPC 标准错误码判断错误是否应该重试
///
/// # Arguments
/// * `status` - gRPC Status 对象
///
/// # Returns
/// 错误分类结果
///
/// # Examples
/// ```
/// use tonic::{Code, Status};
/// use rcoder::grpc::GrpcErrorCategory;
///
/// let status = Status::unavailable("服务不可用");
/// let category = rcoder::grpc::categorize_grpc_error(&status);
/// assert_eq!(category, GrpcErrorCategory::Retryable);
/// ```
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

/// 判断 gRPC 错误是否应该重试
///
/// # Arguments
/// * `status` - gRPC Status 对象
///
/// # Returns
/// `true` 如果错误可以重试，`false` 否则
pub fn should_retry_grpc_error(status: &Status) -> bool {
    matches!(categorize_grpc_error(status), GrpcErrorCategory::Retryable)
}

/// 从 anyhow::Error 中提取 Tonic Status（如果存在）
///
/// # Arguments
/// * `error` - anyhow Error 对象
///
/// # Returns
/// `Some(Status)` 如果错误包含 Tonic Status，`None` 否则
pub fn extract_grpc_status(error: &anyhow::Error) -> Option<&Status> {
    error.downcast_ref::<Status>()
}

/// 判断 anyhow::Error 是否应该重试（自动提取 Tonic Status）
///
/// # Arguments
/// * `error` - anyhow Error 对象
///
/// # Returns
/// `true` 如果错误包含可重试的 gRPC 错误，`false` 否则
pub fn should_retry_error(error: &anyhow::Error) -> bool {
    if let Some(status) = extract_grpc_status(error) {
        should_retry_grpc_error(status)
    } else {
        // 非 gRPC 错误，保守策略：不重试
        false
    }
}

/// 获取错误的友好描述
///
/// # Arguments
/// * `status` - gRPC Status 对象
///
/// # Returns
/// 错误的中文描述
pub fn get_error_description(status: &Status) -> &'static str {
    match status.code() {
        Code::Ok => "成功",
        Code::Cancelled => "操作已取消",
        Code::Unknown => "未知错误",
        Code::InvalidArgument => "参数错误",
        Code::DeadlineExceeded => "请求超时",
        Code::NotFound => "资源未找到",
        Code::AlreadyExists => "资源已存在",
        Code::PermissionDenied => "权限不足",
        Code::ResourceExhausted => "资源耗尽",
        Code::FailedPrecondition => "前置条件失败",
        Code::Aborted => "操作被中止",
        Code::OutOfRange => "超出范围",
        Code::Unimplemented => "方法未实现",
        Code::Internal => "服务器内部错误",
        Code::Unavailable => "服务不可用",
        Code::DataLoss => "数据丢失",
        Code::Unauthenticated => "未认证",
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
    fn test_should_retry_grpc_error() {
        let retryable = Status::unavailable("service unavailable");
        assert!(should_retry_grpc_error(&retryable));

        let non_retryable = Status::invalid_argument("bad request");
        assert!(!should_retry_grpc_error(&non_retryable));
    }
}
