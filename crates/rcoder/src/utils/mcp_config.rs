use std::path::PathBuf;

use agent_client_protocol::{McpServer, EnvVariable};

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
    McpServer::Stdio {
        name: "frontend-template".to_string(),
        command: PathBuf::from("npx"),
        args: vec!["xagi-frontend-mcp@latest".to_string()],
        env: vec![EnvVariable {
            name: "NODE_ENV".to_string(),
            value: "production".to_string(),
            meta: None,
        }],
    }
}

/// 创建默认的 MCP 服务器列表
///
/// 包含 context7 和 xagi-frontend-mcp 等默认 MCP 服务器
pub fn create_default_mcp_servers(_api_key: Option<&str>) -> Vec<McpServer> {
    let mut servers = Vec::new();

    // 添加 context7 MCP 服务器
    servers.push(create_context7_mcp_server(None));
    
    // 添加前端模板 MCP 服务器
    servers.push(create_xagi_frontend_mcp_server());

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
    fn test_create_xagi_frontend_mcp_server() {
        let server = create_xagi_frontend_mcp_server();

        match server {
            McpServer::Stdio {
                name,
                command,
                args,
                env,
            } => {
                assert_eq!(name, "frontend-template");
                assert_eq!(command, std::path::PathBuf::from("npx"));
                assert_eq!(args, vec!["xagi-frontend-mcp@latest".to_string()]);
                assert_eq!(env.len(), 1);
                assert_eq!(env[0].name, "NODE_ENV");
                assert_eq!(env[0].value, "production");
            }
            _ => panic!("Expected Stdio variant"),
        }
    }

    #[test]
    fn test_create_default_mcp_servers() {
        let servers = create_default_mcp_servers(None);

        assert_eq!(servers.len(), 2);

        // 验证 context7 服务器
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

        // 验证 frontend-template 服务器
        match &servers[1] {
            McpServer::Stdio { name, args, env, .. } => {
                assert_eq!(name, "frontend-template");
                assert_eq!(*args, vec!["xagi-frontend-mcp@latest".to_string()]);
                assert_eq!(env.len(), 1);
                assert_eq!(env[0].name, "NODE_ENV");
                assert_eq!(env[0].value, "production");
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
        let has_context7 = servers.iter().any(|server| {
            matches!(server, McpServer::Stdio { name, .. } if name == "context7")
        });
        assert!(has_context7);
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
