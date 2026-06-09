//! Gateway 配置
//!
//! 所有配置通过环境变量注入，适配 K8s ConfigMap + Deployment 模式。

use std::time::Duration;

/// rcoder-gateway 配置
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// 网关监听端口（默认 8090）
    pub gateway_port: u16,
    /// rcoder-control 服务地址（K8s Service FQDN 或 localhost）
    pub control_plane_url: String,
    /// Envoy Gateway 服务地址
    pub envoy_gateway_url: String,
    /// K8s namespace（用于构建 Service FQDN）
    pub namespace: String,
    /// Cluster cache TTL（秒）
    pub cache_ttl_seconds: u64,
}

impl GatewayConfig {
    /// 从环境变量加载配置
    pub fn from_env() -> Self {
        let gateway_port = std::env::var("GATEWAY_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8090);
        let control_plane_url = std::env::var("RCODER_CONTROL_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8087".to_string());
        let envoy_gateway_url = std::env::var("ENVOY_GATEWAY_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
        let namespace = std::env::var("RCODER_K8S_NAMESPACE")
            .unwrap_or_else(|_| "default".to_string());
        let cache_ttl_seconds = std::env::var("CACHE_TTL_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(600);

        Self {
            gateway_port,
            control_plane_url,
            envoy_gateway_url,
            namespace,
            cache_ttl_seconds,
        }
    }

    pub fn cache_ttl(&self) -> Duration {
        Duration::from_secs(self.cache_ttl_seconds)
    }

    /// 从 URL 解析 (host, port)
    pub fn parse_addr(url: &str) -> (&str, u16) {
        let stripped = url.strip_prefix("http://").unwrap_or(url);
        let stripped = stripped.strip_suffix('/').unwrap_or(stripped);
        if let Some((host, port)) = stripped.rsplit_once(':')
            && let Ok(p) = port.parse()
        {
            return (host, p);
        }
        (stripped, 80)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_addr() {
        assert_eq!(GatewayConfig::parse_addr("http://10.0.0.1:8080"), ("10.0.0.1", 8080));
        assert_eq!(GatewayConfig::parse_addr("http://rcoder-control:8087/"), ("rcoder-control", 8087));
        assert_eq!(GatewayConfig::parse_addr("http://localhost"), ("localhost", 80));
    }
}
