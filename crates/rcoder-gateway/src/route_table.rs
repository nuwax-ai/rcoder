//! 路由表定义
//!
//! 使用 matchit radix tree 匹配请求路径，区分数据面和控制面路由。

use matchit::Router;
use shared_types::ServiceType;

/// 路由匹配结果
#[derive(Debug, Clone)]
pub enum RouteType {
    /// 数据面：请求需要路由到 agent_runner Pod（经 Envoy Gateway）
    DataPlane(DataPlaneRoute),
    /// 控制面：请求透传到 rcoder-control
    ControlPlane,
    /// 网关健康检查
    GatewayHealth,
}

/// 数据面路由信息
#[derive(Debug, Clone)]
pub struct DataPlaneRoute {
    /// 标识符字段名（如 "user_id"、"project_id"）
    pub identifier_field: &'static str,
    /// 标识符提取来源
    pub source: IdentifierSource,
    /// 对应的 service_type（ComputerAgentRunner 或 WebAgentRunner）
    pub service_type: ServiceType,
    /// 只读路由（如 GET status），容器不存在时不应触发 pod 创建
    pub read_only: bool,
}

/// 标识符提取来源
#[derive(Debug, Clone)]
pub enum IdentifierSource {
    /// 从 JSON body 提取
    Body,
    /// 从 URL path 命名参数提取（如 "project_id"）
    Path(String),
    /// 通过 session_id 解析
    Session,
}

/// 构建完整路由表
pub fn build_route_table() -> Result<Router<RouteType>, matchit::InsertError> {
    let mut router = Router::new();

    // ── 网关自身 ──
    router.insert("/gateway/health", RouteType::GatewayHealth)?;

    // ── Computer Agent 数据面路由 ──
    let computer_body = |field: &'static str, read_only: bool| {
        RouteType::DataPlane(DataPlaneRoute {
            identifier_field: field,
            source: IdentifierSource::Body,
            service_type: ServiceType::ComputerAgentRunner,
            read_only,
        })
    };

    router.insert("/computer/chat", computer_body("user_id", false))?;
    router.insert("/computer/agent/stop", computer_body("user_id", false))?;
    router.insert(
        "/computer/agent/session/cancel",
        computer_body("user_id", false),
    )?;
    router.insert("/computer/agent/status", computer_body("user_id", true))?; // 只读
    router.insert("/computer/notify-resolved", computer_body("user_id", false))?;

    // SSE progress：session_id 解析（只读，不触发 pod 创建）
    router.insert(
        "/computer/progress/{session_id}",
        RouteType::DataPlane(DataPlaneRoute {
            identifier_field: "session_id",
            source: IdentifierSource::Session,
            service_type: ServiceType::ComputerAgentRunner,
            read_only: true,
        }),
    )?;

    // ── RCoder Agent 数据面路由 ──
    let rcoder_body = |field: &'static str, read_only: bool| {
        RouteType::DataPlane(DataPlaneRoute {
            identifier_field: field,
            source: IdentifierSource::Body,
            service_type: ServiceType::WebAgentRunner,
            read_only,
        })
    };

    router.insert("/chat", rcoder_body("project_id", false))?;
    router.insert("/agent/session/cancel", rcoder_body("project_id", false))?;
    router.insert("/agent/stop", rcoder_body("project_id", false))?;

    // Agent status：path 参数提取（只读）
    router.insert(
        "/agent/status/{project_id}",
        RouteType::DataPlane(DataPlaneRoute {
            identifier_field: "project_id",
            source: IdentifierSource::Path("project_id".to_string()),
            service_type: ServiceType::WebAgentRunner,
            read_only: true,
        }),
    )?;

    // RCoder SSE progress：session_id 解析（只读）
    router.insert(
        "/agent/progress/{session_id}",
        RouteType::DataPlane(DataPlaneRoute {
            identifier_field: "session_id",
            source: IdentifierSource::Session,
            service_type: ServiceType::WebAgentRunner,
            read_only: true,
        }),
    )?;

    // ── Agent Management 数据面路由 ──
    router.insert(
        "/agent-mgmt/agents/{action}",
        RouteType::DataPlane(DataPlaneRoute {
            identifier_field: "project_id",
            source: IdentifierSource::Body,
            service_type: ServiceType::WebAgentRunner,
            read_only: false,
        }),
    )?;

    // ── 控制面路由（不注册，由默认匹配处理）──
    // POST /computer/pod/ensure → ControlPlane (default)
    // POST /computer/pod/restart → ControlPlane (default)
    // POST /computer/pod/stop → ControlPlane (default)
    // POST /computer/pod/keepalive → ControlPlane (default)
    // GET  /computer/pod/count → ControlPlane (default)
    // GET  /health → ControlPlane (default)

    Ok(router)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_route() {
        let router = build_route_table().expect("valid route table");
        let result = router.at("/gateway/health").unwrap();
        assert!(matches!(result.value, RouteType::GatewayHealth));
    }

    #[test]
    fn test_computer_chat_route() {
        let router = build_route_table().expect("valid route table");
        let result = router.at("/computer/chat").unwrap();
        match &result.value {
            RouteType::DataPlane(route) => {
                assert_eq!(route.identifier_field, "user_id");
                assert_eq!(route.service_type, ServiceType::ComputerAgentRunner);
            }
            _ => panic!("expected DataPlane"),
        }
    }

    #[test]
    fn test_computer_progress_session() {
        let router = build_route_table().expect("valid route table");
        let result = router.at("/computer/progress/sess-abc123").unwrap();
        match &result.value {
            RouteType::DataPlane(route) => {
                assert_eq!(route.identifier_field, "session_id");
                assert!(matches!(route.source, IdentifierSource::Session));
            }
            _ => panic!("expected DataPlane"),
        }
    }

    #[test]
    fn test_agent_status_path() {
        let router = build_route_table().expect("valid route table");
        let result = router.at("/agent/status/proj-456").unwrap();
        match &result.value {
            RouteType::DataPlane(route) => {
                assert_eq!(route.identifier_field, "project_id");
                assert!(matches!(&route.source, IdentifierSource::Path(s) if s == "project_id"));
            }
            _ => panic!("expected DataPlane"),
        }
    }

    #[test]
    fn test_control_plane_defaults() {
        let router = build_route_table().expect("valid route table");
        assert!(router.at("/computer/pod/ensure").is_err());
        assert!(router.at("/health").is_err());
        assert!(router.at("/computer/pod/count").is_err());
    }

    #[test]
    fn test_rcoder_chat_route() {
        let router = build_route_table().expect("valid route table");
        let result = router.at("/chat").unwrap();
        match &result.value {
            RouteType::DataPlane(route) => {
                assert_eq!(route.identifier_field, "project_id");
                assert_eq!(route.service_type, ServiceType::WebAgentRunner);
            }
            _ => panic!("expected DataPlane"),
        }
    }
}
