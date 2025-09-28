use std::path::PathBuf;

use agent_client_protocol::McpServer;

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
    McpServer::Stdio {
        name: "context7".to_string(),
        command: PathBuf::from("bunx"),
        args: vec!["-y".to_string(), "@upstash/context7-mcp".to_string()],
        env: Vec::new(), // 不需要额外的环境变量
    }
}

/// 创建默认的 context7 MCP 服务器配置（不使用 API 密钥）
pub fn create_default_context7_mcp_server() -> Option<McpServer> {
    // 不使用 API 密钥，直接创建基本配置
    Some(create_context7_mcp_server(None))
}

/// 创建包含 context7 的 MCP 服务器列表
///
/// 不使用 API key，直接提供基本功能
pub fn create_mcp_servers_with_context7(_api_key: Option<&str>) -> Vec<McpServer> {
    let mut servers = Vec::new();

    // 不使用 API key，直接创建基本配置
    servers.push(create_context7_mcp_server(None));

    servers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_context7_mcp_server() {
        let server = create_context7_mcp_server(None);

        match server {
            McpServer::Stdio {
                name,
                command,
                args,
                env,
            } => {
                assert_eq!(name, "context7");
                assert_eq!(command, std::path::PathBuf::from("bunx"));
                assert_eq!(
                    args,
                    vec!["-y".to_string(), "@upstash/context7-mcp".to_string()]
                );
                assert_eq!(env.len(), 0);
            }
            _ => panic!("Expected Stdio variant"),
        }
    }

    #[test]
    fn test_create_mcp_servers_with_context7() {
        let servers = create_mcp_servers_with_context7(None);

        assert_eq!(servers.len(), 1);

        match &servers[0] {
            McpServer::Stdio { name, args, .. } => {
                assert_eq!(name, "context7");
                assert_eq!(
                    *args,
                    vec!["-y".to_string(), "@upstash/context7-mcp".to_string()]
                );
            }
            _ => panic!("Expected Stdio variant"),
        }
    }

    #[test]
    fn test_create_default_context7_mcp_server() {
        let server = create_default_context7_mcp_server();
        assert!(server.is_some());

        match server.unwrap() {
            McpServer::Stdio { name, args, .. } => {
                assert_eq!(name, "context7");
                assert_eq!(
                    *args,
                    vec!["-y".to_string(), "@upstash/context7-mcp".to_string()]
                );
            }
            _ => panic!("Expected Stdio variant"),
        }
    }
}
