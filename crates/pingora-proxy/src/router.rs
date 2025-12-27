//! 路由配置模块
//!
//! 集中管理所有 Pingora 代理服务的路由定义，方便查看和维护。
//!
//! ## 支持的路由
//!
//! | 路径模式 | 路由类型 | 说明 |
//! |---------|---------|------|
//! | `/computer/vnc/{user_id}/{project_id}/{*path}` | VNC WebSocket 代理 | 代理到容器的 noVNC 服务 (端口 6080) |
//! | `/proxy/{port}/{*path}` | 端口反向代理 | 动态端口路由到后端服务 |
//!
//! ## 路由语法
//!
//! - `{param}`: 命名参数，匹配单个路径段（例如 `{user_id}` 匹配 "user_123"）
//! - `{*path}`: 通配符参数，匹配剩余所有路径（必须在最后）
//!
//! ## 使用示例
//!
//! ```rust
//! use pingora_proxy::router::create_router;
//!
//! let router = create_router();
//! let matched = router.at("/computer/vnc/user_123/proj_456/vnc.html").unwrap();
//! ```

use matchit::Router;

/// 路由类型枚举
///
/// 定义 Pingora 代理支持的所有路由类型，用于类型安全的路由分发
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteType {
    /// VNC WebSocket 代理: `/computer/vnc/{user_id}/{project_id}/{*path}`
    ///
    /// - `user_id`: 用户标识符
    /// - `project_id`: 项目标识符
    /// - `path`: 剩余路径（如 `vnc.html`, `websockify` 等）
    ///
    /// **目标**: 容器内的 noVNC 服务（端口 6080）
    ///
    /// **示例**:
    /// - `/computer/vnc/user_123/proj_456/vnc.html`
    /// - `/computer/vnc/alice/myproject/websockify`
    VncProxy,

    /// 端口反向代理: `/proxy/{port}/{*path}`
    ///
    /// - `port`: 目标后端端口号
    /// - `path`: 剩余路径
    ///
    /// **目标**: 指定端口的后端服务（默认 127.0.0.1）
    ///
    /// **示例**:
    /// - `/proxy/8080/api/status`
    /// - `/proxy/3000/`
    PortProxy,

    /// 健康检查: `/health`
    ///
    /// **功能**: 返回 Pingora 代理服务的健康状态
    ///
    /// **响应**: JSON 格式的健康状态信息
    ///
    /// **示例**:
    /// - `/health` → `{"status":"ok","service":"pingora-proxy"}`
    HealthCheck,
}

/// 创建路由表
///
/// 初始化并配置所有支持的路由规则。
///
/// # 路由优先级
///
/// matchit 使用 radix tree 结构，路由匹配遵循以下优先级：
/// 1. 静态路径段优先于参数
/// 2. 命名参数 `{param}` 优先于通配符 `{*path}`
/// 3. 先注册的路由在同级别时优先
///
/// # 错误处理
///
/// 如果路由配置有冲突，会在插入时 panic（通常在开发阶段就能发现）
///
/// # 返回
///
/// 返回配置好的 `Router<RouteType>`
///
/// # 示例
///
/// ```rust
/// use pingora_proxy::router::{create_router, RouteType};
///
/// let router = create_router();
///
/// // 测试 VNC 路由
/// let matched = router.at("/computer/vnc/user_123/proj_456/vnc.html").unwrap();
/// assert_eq!(*matched.value, RouteType::VncProxy);
/// assert_eq!(matched.params.get("user_id"), Some("user_123"));
///
/// // 测试端口代理路由
/// let matched = router.at("/proxy/8080/api").unwrap();
/// assert_eq!(*matched.value, RouteType::PortProxy);
/// assert_eq!(matched.params.get("port"), Some("8080"));
/// ```
pub fn create_router() -> Router<RouteType> {
    let mut router = Router::new();

    // ========================================================================
    // VNC WebSocket 代理路由
    // ========================================================================
    //
    // 路径格式: /computer/vnc/{user_id}/{project_id}/{*path}
    //
    // 功能: 将 WebSocket 请求代理到用户容器的 noVNC 服务（端口 6080）
    //
    // 参数:
    // - user_id: 用户标识符，用于查找对应的容器 IP
    // - project_id: 项目标识符（用于日志和追踪）
    // - path: 剩余路径，转发到 noVNC 服务
    //
    // 示例:
    // - /computer/vnc/user_123/proj_456/vnc.html -> 容器IP:6080/vnc.html
    // - /computer/vnc/user_123/proj_456/websockify -> 容器IP:6080/websockify (WebSocket)
    //
    router
        .insert(
            "/computer/vnc/{user_id}/{project_id}/{*path}",
            RouteType::VncProxy,
        )
        .expect("Failed to insert VNC proxy route");

    // ========================================================================
    // 端口反向代理路由
    // ========================================================================
    //
    // 路径格式: /proxy/{port}/{*path}
    //
    // 功能: 根据端口号动态路由到后端服务
    //
    // 参数:
    // - port: 目标后端端口号（1-65535）
    // - path: 剩余路径，转发到后端服务
    //
    // 示例:
    // - /proxy/8080/api/status -> 127.0.0.1:8080/api/status
    // - /proxy/3000/ -> 127.0.0.1:3000/
    //
    router
        .insert("/proxy/{port}/{*path}", RouteType::PortProxy)
        .expect("Failed to insert port proxy route");

    // ========================================================================
    // 健康检查路由
    // ========================================================================
    //
    // 路径格式: /health
    //
    // 功能: 返回 Pingora 代理服务的健康状态，用于验证服务是否正常运行
    //
    // 返回: JSON 格式的健康状态
    //
    // 示例:
    // - /health → {"status":"ok","service":"pingora-proxy","timestamp":1234567890}
    //
    router
        .insert("/health", RouteType::HealthCheck)
        .expect("Failed to insert health check route");

    router
}

/// 获取所有路由的文档信息
///
/// 返回人类可读的路由列表，用于调试和文档生成
pub fn get_routes_documentation() -> Vec<(String, String, String)> {
    vec![
        (
            "/computer/vnc/{user_id}/{project_id}/{*path}".to_string(),
            "VNC WebSocket 代理".to_string(),
            "代理到用户容器的 noVNC 服务（端口 6080），支持 WebSocket 升级".to_string(),
        ),
        (
            "/proxy/{port}/{*path}".to_string(),
            "端口反向代理".to_string(),
            "动态路由到指定端口的后端服务".to_string(),
        ),
        (
            "/health".to_string(),
            "健康检查".to_string(),
            "返回 Pingora 代理服务的健康状态".to_string(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_router() {
        let router = create_router();

        // 测试 VNC 路由
        let matched = router
            .at("/computer/vnc/user_123/proj_456/vnc.html")
            .unwrap();
        assert_eq!(*matched.value, RouteType::VncProxy);
        assert_eq!(matched.params.get("user_id"), Some("user_123"));
        assert_eq!(matched.params.get("project_id"), Some("proj_456"));
        assert_eq!(matched.params.get("path"), Some("vnc.html"));

        // 测试端口代理路由
        let matched = router.at("/proxy/8080/api/status").unwrap();
        assert_eq!(*matched.value, RouteType::PortProxy);
        assert_eq!(matched.params.get("port"), Some("8080"));
        assert_eq!(matched.params.get("path"), Some("api/status"));
    }

    #[test]
    fn test_vnc_route_variations() {
        let router = create_router();

        // WebSocket 路径
        let matched = router
            .at("/computer/vnc/user_123/proj_456/websockify")
            .unwrap();
        assert_eq!(*matched.value, RouteType::VncProxy);
        assert_eq!(matched.params.get("path"), Some("websockify"));

        // 多级子路径
        let matched = router
            .at("/computer/vnc/user_123/proj_456/api/v1/status")
            .unwrap();
        assert_eq!(*matched.value, RouteType::VncProxy);
        assert_eq!(matched.params.get("path"), Some("api/v1/status"));
    }

    #[test]
    fn test_port_proxy_route_variations() {
        let router = create_router();

        // 不同端口
        for port in [3000, 8080, 9000, 5173] {
            let path = format!("/proxy/{}/api", port);
            let matched = router.at(&path).unwrap();
            assert_eq!(*matched.value, RouteType::PortProxy);
            assert_eq!(matched.params.get("port"), Some(port.to_string().as_str()));
        }
    }

    #[test]
    fn test_route_not_found() {
        let router = create_router();

        // 不匹配的路径应该返回错误
        assert!(router.at("/api/v1/users").is_err());
        assert!(router.at("/unknown/path").is_err());
        assert!(router.at("/computer/desktop").is_err());
    }

    #[test]
    fn test_get_routes_documentation() {
        let docs = get_routes_documentation();
        assert_eq!(docs.len(), 3);

        // 验证 VNC 路由文档
        assert!(docs[0].0.contains("vnc"));
        assert!(docs[0].1.contains("VNC"));

        // 验证端口代理路由文档
        assert!(docs[1].0.contains("proxy"));
        assert!(docs[1].1.contains("端口"));

        // 验证健康检查路由文档
        assert!(docs[2].0.contains("health"));
        assert!(docs[2].1.contains("健康"));
    }

    #[test]
    fn test_route_type_equality() {
        assert_eq!(RouteType::VncProxy, RouteType::VncProxy);
        assert_eq!(RouteType::PortProxy, RouteType::PortProxy);
        assert_ne!(RouteType::VncProxy, RouteType::PortProxy);
    }

    #[test]
    fn test_route_type_debug() {
        let vnc = RouteType::VncProxy;
        let port = RouteType::PortProxy;

        let vnc_str = format!("{:?}", vnc);
        let port_str = format!("{:?}", port);

        assert!(vnc_str.contains("VncProxy"));
        assert!(port_str.contains("PortProxy"));
    }
}
