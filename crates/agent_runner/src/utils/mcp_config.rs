use std::path::PathBuf;

use agent_client_protocol::{EnvVariable, McpServer, McpServerStdio};

/// 创建 context7 MCP 服务器配置
///
/// 根据提供的配置创建 context7 MCP 服务器：
/// ```json
/// {
///   "mcpServers": {
///     "context7": {
///       "command": "bunx",
///       "args": [
///         "-y",
///         "@upstash/context7-mcp"
///       ]
///     }
///   }
/// }
/// ```
/// 注意：不使用 API key，只提供基本功能
pub fn create_context7_mcp_server(_api_key: Option<&str>) -> McpServer {
    McpServer::Stdio(
        McpServerStdio::new("context7", PathBuf::from("bunx"))
            .args(vec!["-y".to_string(), "@upstash/context7-mcp".to_string()]),
    )
}

/// 创建默认的 context7 MCP 服务器配置（不使用 API 密钥）
pub fn create_default_context7_mcp_server() -> Option<McpServer> {
    // 不使用 API 密钥，直接创建基本配置
    Some(create_context7_mcp_server(None))
}

/// 创建 xagi-frontend-mcp MCP 服务器配置
///
/// 根据提供的配置创建 xagi-frontend-mcp MCP 服务器：
/// ```json
/// {
///   "mcpServers": {
///     "xagi-frontend-mcp": {
///       "command": "npx",
///       "args": [
///         "agi-frontend-mcp@latest"
///       ],
///       "env": {
///         "NODE_ENV": "production"
///       }
///     }
///   }
/// }
/// ```
pub fn create_xagi_frontend_mcp_server() -> McpServer {
    McpServer::Stdio(
        McpServerStdio::new("frontend-template", PathBuf::from("npx"))
            .args(vec!["xagi-frontend-mcp@latest".to_string()])
            .env(vec![EnvVariable::new(
                "NODE_ENV".to_string(),
                "production".to_string(),
            )]),
    )
}

/// 创建 fetch MCP 服务器配置
///
/// 根据提供的配置创建 fetch MCP 服务器：
/// ```json
/// {
///   "mcpServers": {
///     "fetch": {
///       "args": [
///         "mcp-server-fetch"
///       ],
///       "command": "uvx"
///     }
///   }
/// }
/// ```
pub fn create_fetch_mcp_server() -> McpServer {
    McpServer::Stdio(
        McpServerStdio::new("fetch", PathBuf::from("uvx"))
            .args(vec!["mcp-server-fetch".to_string()]),
    )
}

/// 创建默认的 MCP 服务器列表
///
/// 包含 context7、xagi-frontend-mcp 和 fetch 等默认 MCP 服务器
pub fn create_default_mcp_servers(_api_key: Option<&str>) -> Vec<McpServer> {
    let mut servers = Vec::new();

    // 添加 context7 MCP 服务器
    servers.push(create_context7_mcp_server(None));

    // // 添加前端模板 MCP 服务器
    // servers.push(create_xagi_frontend_mcp_server());

    // 添加 fetch MCP 服务器
    servers.push(create_fetch_mcp_server());

    servers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_context7_mcp_server() {
        let server = create_context7_mcp_server(None);

        match server {
            McpServer::Stdio(server) => {
                assert_eq!(server.name, "context7");
                assert_eq!(server.command, std::path::PathBuf::from("bunx"));
                assert_eq!(
                    server.args,
                    vec!["-y".to_string(), "@upstash/context7-mcp".to_string()]
                );
                assert_eq!(server.env.len(), 0);
            }
            _ => panic!("Expected Stdio variant"),
        }
    }

    #[test]
    fn test_create_xagi_frontend_mcp_server() {
        let server = create_xagi_frontend_mcp_server();

        match server {
            McpServer::Stdio(server) => {
                assert_eq!(server.name, "frontend-template");
                assert_eq!(server.command, std::path::PathBuf::from("npx"));
                assert_eq!(server.args, vec!["xagi-frontend-mcp@latest".to_string()]);
                assert_eq!(server.env.len(), 1);
                assert_eq!(server.env[0].name, "NODE_ENV");
                assert_eq!(server.env[0].value, "production");
            }
            _ => panic!("Expected Stdio variant"),
        }
    }

    #[test]
    fn test_create_fetch_mcp_server() {
        let server = create_fetch_mcp_server();

        match server {
            McpServer::Stdio(server) => {
                assert_eq!(server.name, "fetch");
                assert_eq!(server.command, std::path::PathBuf::from("uvx"));
                assert_eq!(server.args, vec!["mcp-server-fetch".to_string()]);
                assert_eq!(server.env.len(), 0);
            }
            _ => panic!("Expected Stdio variant"),
        }
    }

    #[test]
    fn test_create_default_mcp_servers() {
        let servers = create_default_mcp_servers(None);

        assert_eq!(servers.len(), 2); // 现在只有2个服务器，因为 frontend-template 被注释掉了

        // 验证 context7 服务器
        match &servers[0] {
            McpServer::Stdio(server) => {
                assert_eq!(server.name, "context7");
                assert_eq!(
                    server.args,
                    vec!["-y".to_string(), "@upstash/context7-mcp".to_string()]
                );
            }
            _ => panic!("Expected Stdio variant"),
        }

        // 验证 fetch 服务器
        match &servers[1] {
            McpServer::Stdio(server) => {
                assert_eq!(server.name, "fetch");
                assert_eq!(server.command, std::path::PathBuf::from("uvx"));
                assert_eq!(server.args, vec!["mcp-server-fetch".to_string()]);
                assert_eq!(server.env.len(), 0);
            }
            _ => panic!("Expected Stdio variant"),
        }
    }

    // 保持向后兼容的测试函数
    #[test]
    fn test_create_mcp_servers_with_context7() {
        let servers = create_default_mcp_servers(None);

        assert!(servers.len() >= 1);

        // 验证包含 context7 服务器
        let has_context7 = servers
            .iter()
            .any(|server| matches!(server, McpServer::Stdio(server) if server.name == "context7"));
        assert!(has_context7);

        // 验证包含 fetch 服务器
        let has_fetch = servers
            .iter()
            .any(|server| matches!(server, McpServer::Stdio(server) if server.name == "fetch"));
        assert!(has_fetch);
    }

    #[test]
    fn test_create_default_context7_mcp_server() {
        let server = create_default_context7_mcp_server();
        assert!(server.is_some());

        match server.unwrap() {
            McpServer::Stdio(server) => {
                assert_eq!(server.name, "context7");
                assert_eq!(
                    server.args,
                    vec!["-y".to_string(), "@upstash/context7-mcp".to_string()]
                );
            }
            _ => panic!("Expected Stdio variant"),
        }
    }
}
