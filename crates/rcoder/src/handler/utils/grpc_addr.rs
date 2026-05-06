//! gRPC 地址解析工具
//!
//! 从 service_url 提取 gRPC 连接地址。

use crate::AppError;
use shared_types::GRPC_DEFAULT_PORT;
use shared_types::error_codes::ERR_GRPC_ADDR_ERROR;
use tracing::{debug, info};

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
    // 自动添加 scheme 如果缺失（为了能够通过 Url::parse 解析）
    let url_str = if service_url.contains("://") {
        service_url.to_string()
    } else {
        format!("http://{}", service_url)
    };

    let url = url::Url::parse(&url_str).map_err(|e| {
        AppError::with_message(ERR_GRPC_ADDR_ERROR, format!("Invalid service_url: {}", e))
    })?;

    let host = url.host_str().ok_or_else(|| {
        AppError::with_i18n_key(ERR_GRPC_ADDR_ERROR, "error.grpc_service_url_missing_host")
    })?;

    Ok(format!("{}:{}", host, grpc_port))
}

/// 从 service_url 提取 gRPC 地址（使用默认端口）
///
/// # 参数
/// - `service_url`: 服务 URL，格式如 `http://192.168.1.100:8086`
///
/// # 返回
/// 格式化的 gRPC 地址（`host:GRPC_DEFAULT_PORT` 格式）
pub fn extract_grpc_addr(service_url: &str) -> Result<String, AppError> {
    extract_grpc_addr_with_port(service_url, GRPC_DEFAULT_PORT)
}

/// 从 Docker API 实时获取容器 IP（带 5 秒缓存）
///
/// 使用容器名称（如 `computer-agent-runner-user_123`）查询，
/// 因为 container_id 在容器重启后会改变，但 container_name 是稳定的。
///
/// 查询顺序：缓存 → Docker API → 回退到 fallback_ip
///
/// # 参数
/// - `container_name`: 容器名称
/// - `cache`: IP 缓存
/// - `fallback_ip`: 回退 IP 地址
/// - `rcoder_prefix`: RCoder 服务的容器前缀（从配置读取）
/// - `computer_prefix`: ComputerAgentRunner 服务的容器前缀（从配置读取）
pub async fn get_realtime_container_ip_with_cache(
    container_name: &str,
    cache: &crate::grpc::ContainerIpCache,
    fallback_ip: &str,
    rcoder_prefix: &str,
    computer_prefix: &str,
) -> Result<String, String> {
    // 1. 先查缓存
    if let Some(cached_ip) = cache.get(container_name) {
        return Ok(cached_ip);
    }

    // 2. 缓存未命中，查询 Runtime API
    let runtime = docker_manager::runtime::RuntimeManager::get()
        .await
        .map_err(|e| format!("Failed to get runtime: {}", e))?;

    // 使用配置化的前缀，而不是硬编码的 ServiceType::container_prefix()
    let (identifier, service_type) = if let Some(id) =
        container_name.strip_prefix(&format!("{}-", computer_prefix))
    {
        (id, shared_types::ServiceType::ComputerAgentRunner)
    } else if let Some(id) = container_name.strip_prefix(&format!("{}-", rcoder_prefix)) {
        (id, shared_types::ServiceType::RCoder)
    } else {
        return Ok(fallback_ip.to_string());
    };

    // 3. 优先使用 Runtime 查询到的 IP，并写入缓存
    match runtime
        .get_container_info_by_identifier(identifier, &service_type)
        .await
    {
        Ok(Some(info)) if !info.container_ip.is_empty() => {
            cache.insert(container_name.to_string(), info.container_ip.clone());
            Ok(info.container_ip)
        }
        Ok(_) => {
            // 内存映射未命中或 IP 为空，直接通过 Runtime 的 find_container 查询容器 IP
            // find_container 会直接调用 Docker API 获取最新的容器信息，不依赖内存映射
            match runtime.find_container(identifier, &service_type).await {
                Ok(Some(info)) if !info.container_ip.is_empty() => {
                    info!(
                        "🔍 [IP_QUERY] Got container IP via find_container: container_name={}, ip={}",
                        container_name, info.container_ip
                    );
                    cache.insert(container_name.to_string(), info.container_ip.clone());
                    Ok(info.container_ip)
                }
                Ok(_) => {
                    // find_container 返回空或 IP 为空，使用 fallback
                    info!(
                        "⚠️ [IP_QUERY] find_container returned empty, using fallback: container_name={}",
                        container_name
                    );
                    Ok(fallback_ip.to_string())
                }
                Err(e) => Err(format!("runtime find_container failed: {}", e)),
            }
        }
        Err(e) => Err(format!("failed to get container info from runtime: {}", e)),
    }
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

    #[test]
    fn test_extract_missing_scheme() {
        // 测试缺失 scheme，应该被自动补全为 http:// 并解析
        let result = extract_grpc_addr("192.168.1.100:8086").unwrap();
        assert_eq!(result, "192.168.1.100:50051");
    }

    #[test]
    fn test_extract_ipv6() {
        // 测试 IPv6 地址
        let result = extract_grpc_addr("http://[::1]:8086").unwrap();
        assert_eq!(result, "[::1]:50051");
    }

    #[test]
    fn test_extract_malformed() {
        // 测试无效 URL
        let result = extract_grpc_addr("http://");
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_empty() {
        // 测试空字符串
        let result = extract_grpc_addr("");
        assert!(result.is_err());
    }
}
