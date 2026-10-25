//! gRPC 地址解析工具
//!
//! 从 service_url 提取 gRPC 连接地址。

use crate::AppError;
use shared_types::GRPC_DEFAULT_PORT;
use shared_types::ServiceType;
use shared_types::error_codes::ERR_GRPC_ADDR_ERROR;

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

/// Resolve a runtime container/pod name to its business identifier and service type.
///
/// 权威实现在 [`shared_types::container_identity_from_name`]（跨 crate 单一
/// 事实源：SSE 会话注册名、存储记录名、存量无标签容器的纳管识别共用）；
/// 本包装保留引擎侧调用点不变。
pub fn container_identity_from_name<'a>(
    container_name: &'a str,
    rcoder_prefix: &str,
    computer_prefix: &str,
) -> Option<(&'a str, ServiceType)> {
    shared_types::container_identity_from_name(container_name, rcoder_prefix, computer_prefix)
}

/// 从 Docker API 实时获取容器 IP
///
/// 使用容器名称（如 `computer-agent-runner-user_123`）查询，
/// 因为 container_id 在容器重启后会改变，但 container_name 是稳定的。
///
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

    #[test]
    fn test_build_k8s_service_fqdn_format() {
        // 固化 K8s service_url host 契约: {container_name}-svc.{ns}.svc.{domain}
        // 必须与 K8s 实际 Service 名一致(见 docker_manager
        // KubernetesRuntime::get_container_access_address)。
        // 回归钉死: 不能再出现 {prefix}-{prefix}-{id}-svc 双前缀
        // (生产 bug: service_url 多出 rcoder-k8s- 前缀 → permission/cancel/stop transport error)。
        let fqdn = shared_types::build_k8s_service_fqdn(
            "rcoder-computer-agent-runner-1784107191",
            "nuwax-k8s-prod",
            "cluster.local",
        );
        assert_eq!(
            fqdn,
            "rcoder-computer-agent-runner-1784107191-svc.nuwax-k8s-prod.svc.cluster.local"
        );
        // 关键: 仅单层 -svc 后缀, 绝无额外前缀
        assert!(
            !fqdn.contains("rcoder-k8s-"),
            "service FQDN must not carry double prefix, got: {fqdn}"
        );
    }

    #[test]
    fn test_container_identity_uses_longest_configured_prefix_first() {
        let (identifier, service_type) = container_identity_from_name(
            "rcoder-computer-agent-runner-user-1",
            "rcoder",
            "rcoder-computer-agent-runner",
        )
        .unwrap();

        assert_eq!(identifier, "user-1");
        assert_eq!(service_type, ServiceType::ComputerAgentRunner);
    }

    #[test]
    fn test_container_identity_only_uses_configured_prefixes() {
        assert!(
            container_identity_from_name(
                "computer-agent-runner-user-1",
                "rcoder-k8s",
                "rcoder-computer-agent-runner",
            )
            .is_none()
        );

        assert!(
            container_identity_from_name("rcoder-agent-project-1", "rcoder-k8s", "computer")
                .is_none()
        );
    }

    #[test]
    fn test_container_identity_accepts_configured_k8s_prefixes() {
        let (identifier, service_type) = container_identity_from_name(
            "rcoder-computer-agent-runner-user-1",
            "rcoder-k8s",
            "rcoder-computer-agent-runner",
        )
        .unwrap();
        assert_eq!(identifier, "user-1");
        assert_eq!(service_type, ServiceType::ComputerAgentRunner);

        let (identifier, service_type) =
            container_identity_from_name("rcoder-k8s-project-1", "rcoder-k8s", "computer").unwrap();
        assert_eq!(identifier, "project-1");
        assert_eq!(service_type, ServiceType::WebAgentRunner);
    }

    /// userapp 两族反解兜底：builder 先于生产命中（最长前缀），app_id 正确剥离。
    #[test]
    fn test_container_identity_userapp_families() {
        let (id, st) = container_identity_from_name(
            "rcoder-app-builder-23",
            "web-agent-runner",
            "computer-agent-runner",
        )
        .unwrap();
        assert_eq!((id, &st), ("23", &ServiceType::UserappBuilder));

        let (id, st) = container_identity_from_name(
            "rcoder-app-39",
            "web-agent-runner",
            "computer-agent-runner",
        )
        .unwrap();
        assert_eq!((id, &st), ("39", &ServiceType::Userapp));
    }

    /// legacy "rcoder" 前缀配置下，builder/生产名不被 web 前缀吞掉
    /// （"rcoder-app-builder-23" 若被 "rcoder-" 剥走会误判 WebAgentRunner，
    /// identifier 残留 "appbuilder-23"）。
    #[test]
    fn test_container_identity_legacy_prefix_does_not_swallow_userapp() {
        let (id, st) = container_identity_from_name(
            "rcoder-app-builder-23",
            "rcoder",
            "computer-agent-runner",
        )
        .unwrap();
        assert_eq!((id, &st), ("23", &ServiceType::UserappBuilder));

        let (id, st) =
            container_identity_from_name("rcoder-app-39", "rcoder", "computer-agent-runner")
                .unwrap();
        assert_eq!((id, &st), ("39", &ServiceType::Userapp));

        // web 容器名照常解析
        let (id, st) =
            container_identity_from_name("rcoder-project-1", "rcoder", "computer-agent-runner")
                .unwrap();
        assert_eq!((id, &st), ("project-1", &ServiceType::WebAgentRunner));
    }
}
