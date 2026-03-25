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
//! ```rust,ignore
//! use rcoder_proxy::router::create_router;
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

    /// 🔒 API 密钥代理: `/api/{service_name}/{*path}`
    ///
    /// **功能**: 拦截 AI API 请求，注入真实 API 密钥后转发到真实 API 端点
    ///
    /// **参数**:
    /// - `service_name`: 服务名称（如 `anthropic`, `openai`），用于查找密钥配置
    /// - `path`: API 路径（如 `v1/messages`）
    ///
    /// **安全特性**:
    /// - 移除客户端传入的占位密钥
    /// - 从 ApiKeyManager 读取真实密钥并注入请求头
    /// - 重写 URI 到真实 API 端点
    ///
    /// **示例**:
    /// - `/api/anthropic/v1/messages` → `https://api.anthropic.com/v1/messages` (带真实密钥)
    /// - `/api/openai/v1/chat/completions` → `https://api.openai.com/v1/chat/completions`
    ApiProxy,

    /// 🎵 音频流代理: `/computer/audio/{user_id}/{project_id}/{*path}`
    ///
    /// **功能**: 代理到用户容器的音频流服务
    ///
    /// **参数**:
    /// - `user_id`: 用户标识符，用于查找对应的容器 IP
    /// - `project_id`: 项目标识符（用于日志和追踪）
    /// - `path`: 剩余路径
    ///   - `ws` 或 `ws/*`: WebSocket 音频流（端口 6089）
    ///   - 其他: HTTP 静态文件（端口 6090）
    ///
    /// **目标**: 容器内的音频流服务
    /// - HTTP 端口 6090: 静态文件/播放器页面
    /// - WebSocket 端口 6089: Opus 音频流
    ///
    /// **限制**: matchit 的 `{*path}` 通配符要求至少一个字符，尾斜杠路径不匹配
    ///
    /// **示例**:
    /// - `/computer/audio/user_123/proj_456/index.html` → 容器IP:6090/index.html
    /// - `/computer/audio/user_123/proj_456/ws` → 容器IP:6089/ws
    /// - ❌ `/computer/audio/user_123/proj_456/` → 404 (尾斜杠不匹配)
    AudioProxy,

    /// ⌨️ IME 输入法代理: `/computer/ime/{user_id}/{project_id}/{*path}`
    ///
    /// **功能**: 代理到用户容器的 IME 输入法透传服务
    ///
    /// **参数**:
    /// - `user_id`: 用户标识符，用于查找对应的容器 IP
    /// - `project_id`: 项目标识符（用于日志和追踪）
    /// - `path`: 剩余路径（通常为空）
    ///
    /// **目标**: 容器内的 IME 输入法服务（WebSocket 端口 6091）
    ///
    /// **限制**: matchit 的 `{*path}` 通配符要求至少一个字符
    ///
    /// **示例**:
    /// - `/computer/ime/user_123/proj_456/connect` → 容器IP:6091/connect
    /// - ❌ `/computer/ime/user_123/proj_456/` → 404 (尾斜杠不匹配)
    ImeProxy,
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
/// ```rust,ignore
/// use rcoder_proxy::router::{create_router, RouteType};
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
pub fn create_router() -> Result<Router<RouteType>, anyhow::Error> {
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
        .map_err(|e| {
            tracing::error!("[ROUTER] VNC 路由插入失败: {}", e);
            anyhow::anyhow!("VNC route configuration error: {}", e)
        })?;

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
        .map_err(|e| {
            tracing::error!("[ROUTER] 端口代理路由插入失败: {}", e);
            anyhow::anyhow!("Port proxy route configuration error: {}", e)
        })?;

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
        .map_err(|e| {
            tracing::error!("[ROUTER] 健康检查路由插入失败: {}", e);
            anyhow::anyhow!("Health check route configuration error: {}", e)
        })?;

    // ========================================================================
    // 🔒 API 密钥代理路由
    // ========================================================================
    //
    // 路径格式: /api/{service_name}/{*path}
    //
    // 功能: 拦截 AI API 请求，注入真实 API 密钥后转发
    //
    // 安全机制:
    // 1. 客户端使用占位密钥 (sk-placeholder) 发送请求到本地代理
    // 2. Pingora 从 ApiKeyManager 读取真实密钥
    // 3. 移除占位密钥，注入真实密钥到请求头
    // 4. 重写 URI 到真实 API 端点
    //
    // 参数:
    // - service_name: 服务名称（anthropic, openai 等）
    // - path: API 路径（v1/messages, v1/chat/completions 等）
    //
    // 示例:
    // - /api/anthropic/v1/messages -> https://api.anthropic.com/v1/messages (注入真实 x-api-key)
    // - /api/openai/v1/chat/completions -> https://api.openai.com/v1/chat/completions (注入真实 Authorization)
    //
    router
        .insert("/api/{service_name}/{*path}", RouteType::ApiProxy)
        .map_err(|e| {
            tracing::error!("[ROUTER] API 代理路由插入失败: {}", e);
            anyhow::anyhow!("API proxy route configuration error: {}", e)
        })?;

    // ========================================================================
    // 🎵 音频流代理路由
    // ========================================================================
    //
    // 路径格式: /computer/audio/{user_id}/{project_id}/{*path}
    //
    // 功能: 将 HTTP 和 WebSocket 请求代理到用户容器的音频流服务
    //
    // 参数:
    // - user_id: 用户标识符，用于查找对应的容器 IP
    // - project_id: 项目标识符（用于日志和追踪）
    // - path: 剩余路径
    //   - "ws" 或 "ws/*": WebSocket 音频流（端口 6089）
    //   - 其他（包括空路径）: HTTP 静态文件（端口 6090）
    //
    // 示例:
    // - /computer/audio/user_123/proj_456/ -> 容器IP:6090/ (播放器页面)
    // - /computer/audio/user_123/proj_456/ws -> 容器IP:6089/ws (音频流)
    //
    router
        .insert(
            "/computer/audio/{user_id}/{project_id}/{*path}",
            RouteType::AudioProxy,
        )
        .map_err(|e| {
            tracing::error!("[ROUTER] 音频代理路由插入失败: {}", e);
            anyhow::anyhow!("Audio proxy route configuration error: {}", e)
        })?;

    // ========================================================================
    // ⌨️ IME 输入法代理路由
    // ========================================================================
    //
    // 路径格式: /computer/ime/{user_id}/{project_id}/{*path}
    //
    // 功能: 将 WebSocket 请求代理到用户容器的 IME 输入法服务
    //
    // 参数:
    // - user_id: 用户标识符，用于查找对应的容器 IP
    // - project_id: 项目标识符（用于日志和追踪）
    // - path: 剩余路径（通常为空）
    //
    // 示例:
    // - /computer/ime/user_123/proj_456/ -> 容器IP:6091/ (WebSocket)
    //
    router
        .insert(
            "/computer/ime/{user_id}/{project_id}/{*path}",
            RouteType::ImeProxy,
        )
        .map_err(|e| {
            tracing::error!("[ROUTER] IME 代理路由插入失败: {}", e);
            anyhow::anyhow!("IME proxy route configuration error: {}", e)
        })?;

    Ok(router)
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
        (
            "/api/{service_name}/{*path}".to_string(),
            "🔒 API 密钥代理".to_string(),
            "拦截 AI API 请求，注入真实密钥后转发到真实 API 端点".to_string(),
        ),
        (
            "/computer/audio/{user_id}/{project_id}/{*path}".to_string(),
            "🎵 音频流代理".to_string(),
            "代理到用户容器的音频流服务（HTTP 6090 / WebSocket 6089）".to_string(),
        ),
        (
            "/computer/ime/{user_id}/{project_id}/{*path}".to_string(),
            "⌨️ 输入法代理".to_string(),
            "代理到用户容器的 IME 输入法服务（WebSocket 6091）".to_string(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_router() {
        let router = create_router().unwrap();

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
        let router = create_router().unwrap();

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
        let router = create_router().unwrap();

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
        let router = create_router().unwrap();

        // 不匹配的路径应该返回错误
        assert!(router.at("/unknown/path").is_err());
        assert!(router.at("/computer/desktop").is_err());
        // 注意：/api/xxx/yyy 现在会匹配到 ApiProxy 路由
    }

    #[test]
    fn test_api_proxy_route() {
        let router = create_router().unwrap();

        // 测试 Anthropic API 路由
        let matched = router.at("/api/anthropic/v1/messages").unwrap();
        assert_eq!(*matched.value, RouteType::ApiProxy);
        assert_eq!(matched.params.get("service_name"), Some("anthropic"));
        assert_eq!(matched.params.get("path"), Some("v1/messages"));

        // 测试 OpenAI API 路由
        let matched = router.at("/api/openai/v1/chat/completions").unwrap();
        assert_eq!(*matched.value, RouteType::ApiProxy);
        assert_eq!(matched.params.get("service_name"), Some("openai"));
        assert_eq!(matched.params.get("path"), Some("v1/chat/completions"));

        // 测试多级路径
        let matched = router.at("/api/custom/v2/org/project/messages").unwrap();
        assert_eq!(*matched.value, RouteType::ApiProxy);
        assert_eq!(matched.params.get("service_name"), Some("custom"));
        assert_eq!(matched.params.get("path"), Some("v2/org/project/messages"));
    }

    #[test]
    fn test_get_routes_documentation() {
        let docs = get_routes_documentation();
        assert_eq!(docs.len(), 6);

        // 验证 VNC 路由文档
        assert!(docs[0].0.contains("vnc"));
        assert!(docs[0].1.contains("VNC"));

        // 验证端口代理路由文档
        assert!(docs[1].0.contains("proxy"));
        assert!(docs[1].1.contains("端口"));

        // 验证健康检查路由文档
        assert!(docs[2].0.contains("health"));
        assert!(docs[2].1.contains("健康"));

        // 验证 API 代理路由文档
        assert!(docs[3].0.contains("api"));
        assert!(docs[3].1.contains("API"));

        // 验证音频代理路由文档
        assert!(docs[4].0.contains("audio"));
        assert!(docs[4].1.contains("音频"));

        // 验证 IME 代理路由文档
        assert!(docs[5].0.contains("ime"));
        assert!(docs[5].1.contains("输入"));
    }

    #[test]
    fn test_route_type_equality() {
        assert_eq!(RouteType::VncProxy, RouteType::VncProxy);
        assert_eq!(RouteType::PortProxy, RouteType::PortProxy);
        assert_eq!(RouteType::ApiProxy, RouteType::ApiProxy);
        assert_eq!(RouteType::HealthCheck, RouteType::HealthCheck);
        assert_eq!(RouteType::AudioProxy, RouteType::AudioProxy);
        assert_eq!(RouteType::ImeProxy, RouteType::ImeProxy);
        assert_ne!(RouteType::VncProxy, RouteType::PortProxy);
        assert_ne!(RouteType::ApiProxy, RouteType::PortProxy);
        assert_ne!(RouteType::AudioProxy, RouteType::ImeProxy);
    }

    #[test]
    fn test_route_type_debug() {
        let vnc = RouteType::VncProxy;
        let port = RouteType::PortProxy;
        let api = RouteType::ApiProxy;
        let health = RouteType::HealthCheck;
        let audio = RouteType::AudioProxy;
        let ime = RouteType::ImeProxy;

        let vnc_str = format!("{:?}", vnc);
        let port_str = format!("{:?}", port);
        let api_str = format!("{:?}", api);
        let health_str = format!("{:?}", health);
        let audio_str = format!("{:?}", audio);
        let ime_str = format!("{:?}", ime);

        assert!(vnc_str.contains("VncProxy"));
        assert!(port_str.contains("PortProxy"));
        assert!(api_str.contains("ApiProxy"));
        assert!(health_str.contains("HealthCheck"));
        assert!(audio_str.contains("AudioProxy"));
        assert!(ime_str.contains("ImeProxy"));
    }

    #[test]
    fn test_audio_route_matching() {
        let router = create_router().unwrap();

        // 测试音频 WebSocket 路由
        let matched = router
            .at("/computer/audio/user_123/proj_456/ws")
            .unwrap();
        assert_eq!(*matched.value, RouteType::AudioProxy);
        assert_eq!(matched.params.get("user_id"), Some("user_123"));
        assert_eq!(matched.params.get("project_id"), Some("proj_456"));
        assert_eq!(matched.params.get("path"), Some("ws"));

        // 测试音频 HTTP 路由 (带文件名)
        let matched = router
            .at("/computer/audio/user_123/proj_456/index.html")
            .unwrap();
        assert_eq!(*matched.value, RouteType::AudioProxy);
        assert_eq!(matched.params.get("path"), Some("index.html"));

        // 测试带子路径的 WebSocket
        let matched = router
            .at("/computer/audio/user_123/proj_456/ws/token")
            .unwrap();
        assert_eq!(*matched.value, RouteType::AudioProxy);
        assert_eq!(matched.params.get("path"), Some("ws/token"));

        // 注意：尾斜杠路径 (如 "/computer/audio/user_123/proj_456/") 不匹配 {*path} 通配符
        // 这是 matchit 的限制，{*path} 需要至少一个字符
        // 实际场景中客户端通常不会发送尾斜杠到这些路径
    }

    #[test]
    fn test_ime_route_matching() {
        let router = create_router().unwrap();

        // 测试带子路径的 IME 路由
        let matched = router
            .at("/computer/ime/alice/myproject/connect")
            .unwrap();
        assert_eq!(*matched.value, RouteType::ImeProxy);
        assert_eq!(matched.params.get("user_id"), Some("alice"));
        assert_eq!(matched.params.get("project_id"), Some("myproject"));
        assert_eq!(matched.params.get("path"), Some("connect"));

        // 注意：尾斜杠路径不匹配 {*path} 通配符，需要至少一个字符
    }

    #[test]
    fn test_audio_and_ime_route_not_conflict() {
        let router = create_router().unwrap();

        // 确保音频和 IME 路由不会互相干扰
        let audio_matched = router
            .at("/computer/audio/user_123/proj_456/ws")
            .unwrap();
        assert_eq!(*audio_matched.value, RouteType::AudioProxy);

        let ime_matched = router
            .at("/computer/ime/user_123/proj_456/connect")
            .unwrap();
        assert_eq!(*ime_matched.value, RouteType::ImeProxy);

        // 确保不同的路径参数被正确解析
        assert_eq!(audio_matched.params.get("path"), Some("ws"));
        assert_eq!(ime_matched.params.get("path"), Some("connect"));
    }
}
