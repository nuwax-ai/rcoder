//! gRPC 地址解析工具
//!
//! 从 service_url 提取 gRPC 连接地址。

use crate::AppError;
use shared_types::GRPC_DEFAULT_PORT;

/// 从 service_url 提取 gRPC 地址（使用指定端口）
///
/// # 参数
/// - `service_url`: 服务 URL，格式如 `http://192.168.1.100:8086`
/// - `grpc_port`: gRPC 端口号
///
/// # 返回
/// 格式化的 gRPC 地址（`host:port` 格式）
///
/// # 示例
/// ```ignore
/// let addr = extract_grpc_addr_with_port("http://192.168.1.100:8086", 50051)?;
/// assert_eq!(addr, "192.168.1.100:50051");
/// ```
pub fn extract_grpc_addr_with_port(service_url: &str, grpc_port: u16) -> Result<String, AppError> {
    let host = service_url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(':')
        .next()
        .ok_or_else(|| AppError::internal_server_error("无效的 service_url"))?;

    Ok(format!("{}:{}", host, grpc_port))
}

/// 从 service_url 提取 gRPC 地址（使用默认端口）
///
/// # 参数
/// - `service_url`: 服务 URL，格式如 `http://192.168.1.100:8086`
///
/// # 返回
/// 格式化的 gRPC 地址（`host:GRPC_DEFAULT_PORT` 格式）
///
/// # 示例
/// ```ignore
/// let addr = extract_grpc_addr("http://192.168.1.100:8086")?;
/// assert_eq!(addr, "192.168.1.100:50051");
/// ```
pub fn extract_grpc_addr(service_url: &str) -> Result<String, AppError> {
    extract_grpc_addr_with_port(service_url, GRPC_DEFAULT_PORT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_with_http() {
        let result = extract_grpc_addr("http://192.168.1.100:8086").unwrap();
        assert_eq!(result, "192.168.1.100:50051");
    }

    #[test]
    fn test_extract_with_https() {
        let result = extract_grpc_addr("https://example.com:443").unwrap();
        assert_eq!(result, "example.com:50051");
    }

    #[test]
    fn test_extract_with_custom_port() {
        let result = extract_grpc_addr_with_port("http://192.168.1.100:8086", 9999).unwrap();
        assert_eq!(result, "192.168.1.100:9999");
    }

    #[test]
    fn test_extract_without_port_in_url() {
        let result = extract_grpc_addr("http://192.168.1.100").unwrap();
        assert_eq!(result, "192.168.1.100:50051");
    }
}
