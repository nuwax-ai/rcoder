//! RCoder Proxy 实现
//!
//! 提供 Proxy Chain 的基础配置和构建器。
//!
//! ## 设计说明
//!
//! 由于 SACP ProxyToConductor 的复杂性（需要完整的连接上下文），
//! 此模块主要提供：
//! - Proxy 配置管理
//! - MCP 服务器声明构建
//! - 消息转换辅助函数
//!
//! 实际的 Proxy 运行需要在 agent_runner 中与完整的 SACP 连接集成。

use std::sync::Arc;

// 使用 sacp::schema 中的 MCP 服务器类型
use sacp::schema::{
    EnvVariable, HttpHeader, McpServer as SchemaMcpServer, McpServerHttp, McpServerStdio,
};
use tracing::debug;

/// Proxy 配置
#[derive(Debug, Clone, Default)]
pub struct ProxyConfig {
    /// 代理名称
    pub name: String,
    /// 要注入的 MCP 服务器列表
    pub mcp_servers: Vec<McpServerConfig>,
    /// 系统提示词覆盖
    pub system_prompt_override: Option<String>,
    /// 是否启用请求日志
    pub enable_request_logging: bool,
}

/// MCP 服务器配置
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// 服务器名称
    pub name: String,
    /// 服务器描述
    pub description: Option<String>,
    /// 传输类型 (stdio, http, acp)
    pub transport: McpTransportType,
}

/// MCP 传输类型
#[derive(Debug, Clone)]
pub enum McpTransportType {
    /// Stdio 传输（本地进程）
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    /// HTTP 传输（远程服务）
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
    /// ACP 传输（通过 Conductor）
    Acp { uuid: String },
}

/// RCoder Proxy 构建器
pub struct RCoderProxyBuilder {
    config: ProxyConfig,
}

impl RCoderProxyBuilder {
    /// 创建新的构建器
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            config: ProxyConfig {
                name: name.into(),
                ..Default::default()
            },
        }
    }

    /// 添加 MCP 服务器
    pub fn with_mcp_server(mut self, server: McpServerConfig) -> Self {
        self.config.mcp_servers.push(server);
        self
    }

    /// 设置系统提示词覆盖
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.config.system_prompt_override = Some(prompt.into());
        self
    }

    /// 启用请求日志
    pub fn with_request_logging(mut self, enable: bool) -> Self {
        self.config.enable_request_logging = enable;
        self
    }

    /// 构建 RCoderProxy
    pub fn build(self) -> RCoderProxy {
        RCoderProxy {
            config: Arc::new(self.config),
        }
    }
}

/// RCoder Proxy
///
/// 提供 Proxy 配置和 MCP 服务器声明构建功能。
///
/// 主要用途：
/// - 管理要注入的 MCP 服务器配置
/// - 生成符合 SACP 协议的 McpServer 声明
/// - 提供系统提示词覆盖
#[derive(Clone)]
pub struct RCoderProxy {
    config: Arc<ProxyConfig>,
}

impl RCoderProxy {
    /// 创建新的构建器
    pub fn builder(name: impl Into<String>) -> RCoderProxyBuilder {
        RCoderProxyBuilder::new(name)
    }

    /// 获取配置
    pub fn config(&self) -> &ProxyConfig {
        &self.config
    }

    /// 获取代理名称
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// 获取系统提示词覆盖
    pub fn system_prompt(&self) -> Option<&str> {
        self.config.system_prompt_override.as_deref()
    }

    /// 是否启用请求日志
    pub fn is_logging_enabled(&self) -> bool {
        self.config.enable_request_logging
    }

    /// 构建要注入的 MCP 服务器声明列表
    ///
    /// 将内部 `McpServerConfig` 转换为 SACP 协议的 `McpServer` 类型
    pub fn build_mcp_servers(&self) -> Vec<SchemaMcpServer> {
        self.config
            .mcp_servers
            .iter()
            .map(|config| self.build_mcp_server(config))
            .collect()
    }

    /// 构建单个 MCP 服务器声明
    fn build_mcp_server(&self, config: &McpServerConfig) -> SchemaMcpServer {
        match &config.transport {
            McpTransportType::Stdio { command, args, env } => {
                debug!(
                    "[RCoderProxy] 构建 Stdio MCP 服务器: name={}, command={}",
                    config.name, command
                );

                let mut stdio = McpServerStdio::new(config.name.clone(), command.clone());

                if !args.is_empty() {
                    stdio = stdio.args(args.clone());
                }

                if !env.is_empty() {
                    let env_vars: Vec<EnvVariable> = env
                        .iter()
                        .map(|(name, value)| EnvVariable::new(name.clone(), value.clone()))
                        .collect();
                    stdio = stdio.env(env_vars);
                }

                SchemaMcpServer::Stdio(stdio)
            }
            McpTransportType::Http { url, headers } => {
                debug!(
                    "[RCoderProxy] 构建 HTTP MCP 服务器: name={}, url={}",
                    config.name, url
                );

                let mut http = McpServerHttp::new(config.name.clone(), url.clone());

                if !headers.is_empty() {
                    let http_headers: Vec<HttpHeader> = headers
                        .iter()
                        .map(|(name, value)| HttpHeader::new(name.clone(), value.clone()))
                        .collect();
                    http = http.headers(http_headers);
                }

                SchemaMcpServer::Http(http)
            }
            McpTransportType::Acp { uuid } => {
                debug!(
                    "[RCoderProxy] 构建 ACP MCP 服务器: name={}, uuid={}",
                    config.name, uuid
                );
                // ACP 传输使用特殊的 acp:UUID URL 格式
                let http = McpServerHttp::new(config.name.clone(), format!("acp:{}", uuid));
                SchemaMcpServer::Http(http)
            }
        }
    }

    /// 将 MCP 服务器注入到现有列表中
    ///
    /// 用于修改 NewSessionRequest 中的 mcp_servers 字段
    pub fn inject_mcp_servers(&self, existing: &mut Vec<SchemaMcpServer>) {
        let new_servers = self.build_mcp_servers();
        if !new_servers.is_empty() {
            debug!(
                "[RCoderProxy] 注入 {} 个 MCP 服务器",
                new_servers.len()
            );
            existing.extend(new_servers);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_builder() {
        let proxy = RCoderProxy::builder("test-proxy")
            .with_system_prompt("You are a helpful assistant.")
            .with_request_logging(true)
            .with_mcp_server(McpServerConfig {
                name: "test-server".to_string(),
                description: Some("Test MCP server".to_string()),
                transport: McpTransportType::Stdio {
                    command: "node".to_string(),
                    args: vec!["server.js".to_string()],
                    env: vec![],
                },
            })
            .build();

        assert_eq!(proxy.name(), "test-proxy");
        assert_eq!(
            proxy.system_prompt(),
            Some("You are a helpful assistant.")
        );
        assert!(proxy.is_logging_enabled());
        assert_eq!(proxy.config.mcp_servers.len(), 1);
    }

    #[test]
    fn test_build_mcp_servers_stdio() {
        let proxy = RCoderProxy::builder("test")
            .with_mcp_server(McpServerConfig {
                name: "stdio-server".to_string(),
                description: None,
                transport: McpTransportType::Stdio {
                    command: "node".to_string(),
                    args: vec!["index.js".to_string()],
                    env: vec![("NODE_ENV".to_string(), "production".to_string())],
                },
            })
            .build();

        let servers = proxy.build_mcp_servers();
        assert_eq!(servers.len(), 1);

        // 验证是 Stdio 变体
        assert!(matches!(&servers[0], SchemaMcpServer::Stdio(_)));
    }

    #[test]
    fn test_build_mcp_servers_http() {
        let proxy = RCoderProxy::builder("test")
            .with_mcp_server(McpServerConfig {
                name: "http-server".to_string(),
                description: None,
                transport: McpTransportType::Http {
                    url: "http://localhost:8080".to_string(),
                    headers: vec![("Authorization".to_string(), "Bearer token".to_string())],
                },
            })
            .build();

        let servers = proxy.build_mcp_servers();
        assert_eq!(servers.len(), 1);

        // 验证是 Http 变体
        assert!(matches!(&servers[0], SchemaMcpServer::Http(_)));
    }

    #[test]
    fn test_build_mcp_servers_acp() {
        let proxy = RCoderProxy::builder("test")
            .with_mcp_server(McpServerConfig {
                name: "acp-server".to_string(),
                description: None,
                transport: McpTransportType::Acp {
                    uuid: "550e8400-e29b-41d4-a716-446655440000".to_string(),
                },
            })
            .build();

        let servers = proxy.build_mcp_servers();
        assert_eq!(servers.len(), 1);

        // ACP 使用 Http 变体，URL 格式为 acp:UUID
        if let SchemaMcpServer::Http(http) = &servers[0] {
            assert!(http.url.starts_with("acp:"));
        } else {
            panic!("Expected Http variant for ACP transport");
        }
    }

    #[test]
    fn test_inject_mcp_servers() {
        let proxy = RCoderProxy::builder("test")
            .with_mcp_server(McpServerConfig {
                name: "injected".to_string(),
                description: None,
                transport: McpTransportType::Stdio {
                    command: "test".to_string(),
                    args: vec![],
                    env: vec![],
                },
            })
            .build();

        let existing_server = McpServerStdio::new("existing".to_string(), "existing-cmd".to_string());
        let mut existing = vec![SchemaMcpServer::Stdio(existing_server)];

        proxy.inject_mcp_servers(&mut existing);

        assert_eq!(existing.len(), 2);
    }
}
