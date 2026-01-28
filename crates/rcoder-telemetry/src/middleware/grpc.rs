//! gRPC 指标拦截器
//!
//! 为 Tonic gRPC 服务提供指标收集功能。

use std::time::Instant;
use tonic::{Request, Status};

use crate::prometheus::{record_grpc_duration, record_grpc_request};

/// gRPC 指标拦截器
///
/// 用于记录 gRPC 请求的指标。
///
/// # Example
///
/// ```no_run
/// use rcoder_telemetry::middleware::GrpcMetricsInterceptor;
/// use tonic::transport::Server;
///
/// // 创建拦截器
/// let interceptor = GrpcMetricsInterceptor::new();
///
/// // 在服务中使用
/// // Server::builder()
/// //     .add_service(MyServiceServer::with_interceptor(service, interceptor.intercept()))
/// //     .serve(addr)
/// //     .await?;
/// ```
#[derive(Debug, Clone, Default)]
pub struct GrpcMetricsInterceptor;

impl GrpcMetricsInterceptor {
    /// 创建新的 gRPC 指标拦截器
    pub fn new() -> Self {
        Self
    }

    /// 获取拦截器函数
    ///
    /// 返回一个可以传递给 `with_interceptor` 的函数。
    pub fn intercept(&self) -> impl Fn(Request<()>) -> Result<Request<()>, Status> + Clone {
        move |req: Request<()>| {
            // 拦截器只能记录请求开始，响应指标需要在服务层处理
            // 这里主要用于注入 trace context
            Ok(req)
        }
    }
}

/// gRPC 请求计时器
///
/// 用于在服务方法中记录请求耗时。
///
/// # Example
///
/// ```no_run
/// use rcoder_telemetry::middleware::grpc::GrpcRequestTimer;
///
/// async fn my_grpc_method() {
///     let timer = GrpcRequestTimer::new("MyService/MyMethod");
///
///     // ... 处理请求 ...
///
///     timer.record_success();
///     // 或者
///     // timer.record_error();
/// }
/// ```
pub struct GrpcRequestTimer {
    method: String,
    start: Instant,
}

impl GrpcRequestTimer {
    /// 创建新的请求计时器
    pub fn new(method: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            start: Instant::now(),
        }
    }

    /// 记录成功请求
    pub fn record_success(self) {
        let duration = self.start.elapsed();
        record_grpc_request(&self.method, "ok");
        record_grpc_duration(&self.method, duration);
    }

    /// 记录失败请求
    pub fn record_error(self) {
        let duration = self.start.elapsed();
        record_grpc_request(&self.method, "error");
        record_grpc_duration(&self.method, duration);
    }

    /// 记录请求（指定状态）
    pub fn record(self, status: &str) {
        let duration = self.start.elapsed();
        record_grpc_request(&self.method, status);
        record_grpc_duration(&self.method, duration);
    }
}

/// 从 gRPC 请求中提取方法名
///
/// # Arguments
///
/// * `uri` - 请求 URI（如 `/package.Service/Method`）
///
/// # Returns
///
/// 返回方法名（如 `Service/Method`）
pub fn extract_method_name(uri: &str) -> String {
    // gRPC URI 格式: /package.Service/Method
    // 提取 Service/Method 部分
    if let Some(pos) = uri.rfind('.') {
        uri[pos + 1..].to_string()
    } else if let Some(stripped) = uri.strip_prefix('/') {
        stripped.to_string()
    } else {
        uri.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_method_name() {
        assert_eq!(
            extract_method_name("/agent.AgentService/Chat"),
            "AgentService/Chat"
        );
        assert_eq!(
            extract_method_name("/MyService/MyMethod"),
            "MyService/MyMethod"
        );
    }

    #[test]
    fn test_grpc_metrics_interceptor() {
        let interceptor = GrpcMetricsInterceptor::new();
        let intercept_fn = interceptor.intercept();

        // 测试拦截器不会修改请求
        let req = Request::new(());
        let result = intercept_fn(req);
        assert!(result.is_ok());
    }
}
