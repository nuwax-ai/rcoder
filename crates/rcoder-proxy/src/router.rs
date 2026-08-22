//! 路由配置模块
//!
//! 集中管理所有 Pingora 代理服务的路由定义，方便查看和维护。
//!
//! ## 支持的路由
//!
//! | 路径模式 | 路由类型 | 说明 |
//! |---------|---------|------|
//! | `/computer/vnc/{user_id}/{project_id}/{*path}` | VNC WebSocket 代理 | 代理到容器的 noVNC 服务 (端口 6080) |
//! | `/computer/ttyd/{user_id}/{project_id}/{*path}` | ttyd Web 终端代理 | 代理到容器的 ttyd 服务 (端口 7681) |
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

    /// app 专用端口反向代理: `/proxy/apps/{app_id}/{port}/{*path}`
    ///
    /// - `app_id`: 应用 ID（定位具体 app，解决多 app 同端口冲突）
    /// - `port`: app 的 HTTP 端口
    /// - `path`: 剩余路径
    ///
    /// **目标**: app_manager 部署的应用（K8s→`{app_id}-svc:{port}`，Docker→container_ip:{port}）
    ///
    /// **示例**: `/proxy/apps/app-1a2b3c4d/8080/api/users`
    AppPortProxy,

    /// 开发阶段端口反向代理: `/proxy/devapps/{user_id}/{app_id}/{port}/{*path}`
    ///
    /// - `user_id`: 用户 ID（不参与解析；日志排障/归属鉴权锚点）
    /// - `app_id`: 应用 ID（动态解析该 app 的开发容器（UserAppBuilder，per-app）IP）
    /// - `port`: 开发容器内端口（dev server 的 PortPool 端口或自装 pingap 的 9080）
    /// - `path`: 剩余路径
    ///
    /// **目标**: 该 app 开发容器（UserAppBuilder）的同端口——与 AppPortProxy
    /// （部署后 → app 运行容器）对称的开发预览入口。
    ///
    /// **示例**: `/proxy/devapps/6/app-1a2b3c4d/4000/api/users`
    DevPortProxy,

    /// userApp 开发域终端代理族: `/proxy/userapp/{ttyd,vnc,audio,ime}/{app_id}/{*path}`
    ///
    /// 按 **app_id** 定位该 app 的 UserAppBuilder 开发容器（镜像同款：ttyd/noVNC/
    /// 音频/IME 全套），与 computer 族（user_id 定位沙箱）对称的开发场景入口。
    ///
    /// **ttyd**: `/proxy/userapp/ttyd/{app_id}/{*path}` → 容器 ws_terminal(17681) → ttyd 本体；
    ///   终端 cwd = 开发卷 `{USERAPP_WORKSPACE_ROOT}/{app_id}`（X-Ttyd-Service-Type 注入）
    /// **vnc**: `/proxy/userapp/vnc/{app_id}/{*path}` → 容器 noVNC(6080, HTTP+WS)
    /// **audio**: `/proxy/userapp/audio/{app_id}/{*path}` → ws* 6089 流 / 其余 6090 静态
    /// **ime**: `/proxy/userapp/ime/{app_id}/{*path}` → 容器 IME(6091, WebSocket)
    ///
    /// 定位走 find_by_project_id(app_id, UserAppBuilder)（注册表），miss → 404
    /// （提示先创建 workspace）；不走 vnc_backends（user_id 键空间，防撞键）。
    DevTtydProxy,
    DevVncProxy,
    DevAudioProxy,
    DevImeProxy,

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

    /// 🖥️ ttyd Web 终端代理: `/computer/ttyd/{user_id}/{project_id}/{*path}`
    ///
    /// **功能**: 代理到用户容器的 ttyd Web 终端服务（HTTP + WebSocket 同端口 7681）
    ///
    /// **参数**:
    /// - `user_id`: 用户标识符，用于查找容器 IP
    /// - `project_id`: 项目标识符（用于日志/追踪）
    /// - `path`: 剩余路径
    ///   - `ws` 或 `ws/*` → WebSocket
    ///   - 其他 → HTTP（ttyd-index.html 等静态资源）
    ///
    /// **目标**: 容器内 ttyd 服务（端口 7681，libwebsockets 自动按 Upgrade 头分发）
    ///
    /// **WebSocket 子协议**: 客户端必须传 `Sec-WebSocket-Protocol: tty`，
    ///                          pingora 默认透传，ttyd 端按子协议识别客户端
    ///
    /// **示例**:
    /// - `/computer/ttyd/user_123/proj_456/`      → 容器IP:7681/      (ttyd-index.html)
    /// - `/computer/ttyd/user_123/proj_456/ws`    → 容器IP:7681/ws    (WebSocket)
    /// - `/computer/ttyd/user_123/proj_456/ws/token` → 容器IP:7681/ws/token
    TtydProxy,

    /// 🖥️ Web ttyd 终端代理: /web/ttyd/{user_id}/{project_id}/{*path}
    ///
    /// **功能**: 代理到 rcoder 主服务自身的 ttyd 服务（HTTP + WebSocket 同端口 7681）
    ///
    /// **参数**:
    /// - `user_id`: 用户标识符（用于日志和追踪）
    /// - `project_id`: 项目标识符（用于设置工作目录）
    /// - `path`: 剩余路径
    ///   - `ws` 或 `ws/*` → WebSocket
    ///   - 其他 → HTTP（ttyd-index.html 等静态资源）
    ///
    /// **目标**: 127.0.0.1:7681（rcoder-master 容器内的 ttyd 服务）
    ///
    /// **工作目录**: 通过 `--cwd` 参数设置为 `/app/project_workspace/{project_id}`
    ///
    /// **示例**:
    /// - `/web/ttyd/user_123/proj_456/`         → 127.0.0.1:7681/ (ttyd-index.html, --cwd=/app/project_workspace/proj_456)
    /// - `/web/ttyd/user_123/proj_456/ws`       → 127.0.0.1:7681/ws (WebSocket)
    /// - `/web/ttyd/user_123/proj_456/ws/token` → 127.0.0.1:7681/ws/token
    WebTtydProxy,
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
pub fn create_router() -> Result<Router<RouteType>, crate::ProxyError> {
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
            tracing::error!("[ROUTER] VNC route config failed: {}", e);
            crate::ProxyError::RouteConfig(format!("VNC route configuration error: {}", e))
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
            tracing::error!("[ROUTER] port proxy route config failed: {}", e);
            crate::ProxyError::RouteConfig(format!("Port proxy route configuration error: {}", e))
        })?;

    // 兜底：访问 app 根路径 /proxy/{port}（无尾随 path，如 http://host:port/proxy/80）。
    // matchit 的 {*path} 要求至少 1 个字符，/proxy/{port}/{*path} 无法匹配 2 段路径，
    // 单独注册 /proxy/{port} 让根路径访问能命中 PortProxy（handler L48-52 已把空 path
    // 归一为 "/"，转发到 backend 根），否则 /proxy/80 → route not found → 404，
    // app 根路径完全不可达。
    router
        .insert("/proxy/{port}", RouteType::PortProxy)
        .map_err(|e| {
            tracing::error!("[ROUTER] port proxy root route config failed: {}", e);
            crate::ProxyError::RouteConfig(format!(
                "Port proxy root route configuration error: {}",
                e
            ))
        })?;

    // app 专用端口代理（按 app_id+port 路由，解决多 app 同端口冲突）：
    // /proxy/apps/{user_id}/{app_id}/{port}/{path} -> 对应 app 的后端（app_backends 表;
    // user_id 不参与解析, 与 devapps 四段形态统一, 未来归属鉴权锚点）
    router
        .insert(
            "/proxy/apps/{user_id}/{app_id}/{port}/{*path}",
            RouteType::AppPortProxy,
        )
        .map_err(|e| {
            tracing::error!("[ROUTER] app port proxy route config failed: {}", e);
            crate::ProxyError::RouteConfig(format!(
                "App port proxy route configuration error: {}",
                e
            ))
        })?;
    // 兜底：/proxy/apps/{user_id}/{app_id}/{port}（无尾随 path）
    router
        .insert(
            "/proxy/apps/{user_id}/{app_id}/{port}",
            RouteType::AppPortProxy,
        )
        .map_err(|e| {
            tracing::error!("[ROUTER] app port proxy root route config failed: {}", e);
            crate::ProxyError::RouteConfig(format!(
                "App port proxy root route configuration error: {}",
                e
            ))
        })?;
    // 开发阶段代理: /proxy/devapps/{user_id}/{app_id}/{port}/{path} -> 该 app 开发容器(UserAppBuilder)同端口
    router
        .insert(
            "/proxy/devapps/{user_id}/{app_id}/{port}/{*path}",
            RouteType::DevPortProxy,
        )
        .map_err(|e| {
            tracing::error!("[ROUTER] devapps proxy route config failed: {}", e);
            crate::ProxyError::RouteConfig(format!(
                "Devapps proxy route configuration error: {}",
                e
            ))
        })?;
    // 兜底：/proxy/devapps/{user_id}/{app_id}/{port}（无尾随 path）
    router
        .insert(
            "/proxy/devapps/{user_id}/{app_id}/{port}",
            RouteType::DevPortProxy,
        )
        .map_err(|e| {
            tracing::error!("[ROUTER] devapps proxy root route config failed: {}", e);
            crate::ProxyError::RouteConfig(format!(
                "Devapps proxy root route configuration error: {}",
                e
            ))
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
    // userApp 开发域终端/桌面代理族（与 computer 族对称，app_id 定位 UserAppBuilder 开发容器）。
    // 兜底路由（无尾随 path）与通配成对注册——matchit 的 {*path} 至少 1 字符。
    for (prefix, route) in [
        (
            "/proxy/userapp/ttyd/{app_id}/{*path}",
            RouteType::DevTtydProxy,
        ),
        ("/proxy/userapp/ttyd/{app_id}", RouteType::DevTtydProxy),
        (
            "/proxy/userapp/vnc/{app_id}/{*path}",
            RouteType::DevVncProxy,
        ),
        ("/proxy/userapp/vnc/{app_id}", RouteType::DevVncProxy),
        (
            "/proxy/userapp/audio/{app_id}/{*path}",
            RouteType::DevAudioProxy,
        ),
        ("/proxy/userapp/audio/{app_id}", RouteType::DevAudioProxy),
        (
            "/proxy/userapp/ime/{app_id}/{*path}",
            RouteType::DevImeProxy,
        ),
        ("/proxy/userapp/ime/{app_id}", RouteType::DevImeProxy),
    ] {
        router.insert(prefix, route).map_err(|e| {
            tracing::error!("[ROUTER] userapp dev terminal route {prefix} config failed: {e}");
            crate::ProxyError::RouteConfig(format!(
                "userapp dev terminal route {prefix} configuration error: {e}"
            ))
        })?;
    }

    router
        .insert("/health", RouteType::HealthCheck)
        .map_err(|e| {
            tracing::error!("[ROUTER] health check route config failed: {}", e);
            crate::ProxyError::RouteConfig(format!("Health check route configuration error: {}", e))
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
            tracing::error!("[ROUTER] API proxy route config failed: {}", e);
            crate::ProxyError::RouteConfig(format!("API proxy route configuration error: {}", e))
        })?;

    // Fallback: 匹配 /api/{service_name}（无额外路径段）
    // 用于 Claude Code 启动时对 base URL 的 HEAD 连通性检查
    router
        .insert("/api/{service_name}", RouteType::ApiProxy)
        .map_err(|e| {
            tracing::error!("[ROUTER] API proxy fallback route config failed: {}", e);
            crate::ProxyError::RouteConfig(format!(
                "API proxy fallback route configuration error: {}",
                e
            ))
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
            tracing::error!("[ROUTER] audio proxy route config failed: {}", e);
            crate::ProxyError::RouteConfig(format!("Audio proxy route configuration error: {}", e))
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
            tracing::error!("[ROUTER] IME proxy route config failed: {}", e);
            crate::ProxyError::RouteConfig(format!("IME proxy route configuration error: {}", e))
        })?;

    // ========================================================================
    // 🖥️ ttyd Web 终端代理路由
    // ========================================================================
    //
    // 路径格式: /computer/ttyd/{user_id}/{project_id}/{*path}
    //
    // 功能: 代理到用户容器的 ttyd 服务（端口 7681，单端口双协议）
    //       ttyd 用 libwebsockets 同端口监听 HTTP 和 WebSocket，
    //       pingora 默认透传 Connection: Upgrade 头，service 层无需特殊处理
    //
    // 示例:
    // - /computer/ttyd/user_123/proj_456/        → 容器IP:7681/        (ttyd-index.html)
    // - /computer/ttyd/user_123/proj_456/ws     → 容器IP:7681/ws     (WebSocket)
    //
    router
        .insert(
            "/computer/ttyd/{user_id}/{project_id}/{*path}",
            RouteType::TtydProxy,
        )
        .map_err(|e| {
            tracing::error!("[ROUTER] ttyd route config failed: {}", e);
            crate::ProxyError::RouteConfig(format!("ttyd route configuration error: {}", e))
        })?;

    // Fallback: 匹配 /computer/ttyd/{user_id}/{project_id}（无额外路径段）
    // matchit 的 {*path} 通配符需要至少一个字符，尾斜杠路径不匹配
    router
        .insert(
            "/computer/ttyd/{user_id}/{project_id}",
            RouteType::TtydProxy,
        )
        .map_err(|e| {
            tracing::error!("[ROUTER] ttyd fallback route config failed: {}", e);
            crate::ProxyError::RouteConfig(format!(
                "ttyd fallback route configuration error: {}",
                e
            ))
        })?;

    // ========================================================================
    // 🖥️ Web ttyd 终端代理路由
    // ========================================================================
    //
    // 路径格式: /web/ttyd/{user_id}/{project_id}/{*path}
    //
    // 功能: 代理到 rcoder 主服务自身的 ttyd 服务（端口 7681，单端口双协议）
    //       与 agent-runner 的 ttyd 代理不同，这里代理到本地 127.0.0.1:7681
    //       并通过 --cwd 参数设置工作目录为 /app/project_workspace/{project_id}
    //
    // 示例:
    // - /web/ttyd/user_123/proj_456/        → 127.0.0.1:7681/ (ttyd-index.html)
    // - /web/ttyd/user_123/proj_456/ws     → 127.0.0.1:7681/ws (WebSocket)
    //
    router
        .insert(
            "/web/ttyd/{user_id}/{project_id}/{*path}",
            RouteType::WebTtydProxy,
        )
        .map_err(|e| {
            tracing::error!("[ROUTER] Web ttyd route config failed: {}", e);
            crate::ProxyError::RouteConfig(format!("Web ttyd route configuration error: {}", e))
        })?;

    // Fallback: 匹配 /web/ttyd/{user_id}/{project_id}（无额外路径段）
    // matchit 的 {*path} 通配符需要至少一个字符，尾斜杠路径不匹配
    router
        .insert("/web/ttyd/{user_id}/{project_id}", RouteType::WebTtydProxy)
        .map_err(|e| {
            tracing::error!("[ROUTER] Web ttyd fallback route config failed: {}", e);
            crate::ProxyError::RouteConfig(format!(
                "Web ttyd fallback route configuration error: {}",
                e
            ))
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
            "VNC WebSocket proxy".to_string(),
            "Proxy to user's container noVNC service (port 6080), supports WebSocket upgrade"
                .to_string(),
        ),
        (
            "/proxy/{port}/{*path}".to_string(),
            "Port reverse proxy".to_string(),
            "Dynamic routing to backend service on specified port".to_string(),
        ),
        (
            "/proxy/{port}".to_string(),
            "Port reverse proxy (root)".to_string(),
            "Fallback for app root path (no trailing path); matchit {*path} requires >=1 char"
                .to_string(),
        ),
        (
            "/health".to_string(),
            "Health check".to_string(),
            "Returns Pingora proxy service health status".to_string(),
        ),
        (
            "/api/{service_name}/{*path}".to_string(),
            "🔒 API key proxy".to_string(),
            "Intercept AI API requests, inject real key and forward to actual API endpoint"
                .to_string(),
        ),
        (
            "/api/{service_name}".to_string(),
            "🔒 API key proxy (fallback)".to_string(),
            "Fallback for API proxy health check (HEAD requests without path suffix)".to_string(),
        ),
        (
            "/proxy/devapps/{user_id}/{app_id}/{port}/{*path}".to_string(),
            "🧪 Dev apps port proxy".to_string(),
            "Proxy to user's sandbox dev server at the given port (dev preview, zero-registration)"
                .to_string(),
        ),
        (
            "/computer/audio/{user_id}/{project_id}/{*path}".to_string(),
            "🎵 Audio stream proxy".to_string(),
            "Proxy to user's container audio stream service (HTTP 6090 / WebSocket 6089)"
                .to_string(),
        ),
        (
            "/computer/ime/{user_id}/{project_id}/{*path}".to_string(),
            "⌨️ IME proxy".to_string(),
            "Proxy to user's container IME input service (WebSocket 6091)".to_string(),
        ),
        (
            "/computer/ttyd/{user_id}/{project_id}/{*path}".to_string(),
            "🖥️ ttyd web terminal proxy".to_string(),
            "Proxy to user's container ttyd service (single port 7681, HTTP + WebSocket)"
                .to_string(),
        ),
        (
            "/web/ttyd/{user_id}/{project_id}/{*path}".to_string(),
            "🖥️ Web ttyd terminal proxy".to_string(),
            "Proxy to rcoder main service ttyd (127.0.0.1:7681, --cwd=/app/project_workspace/{project_id})".to_string(),
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

        // 测试 app 代理路由（四段：user_id 不参与解析）
        let matched = router.at("/proxy/apps/u6/app-abc/8080/api/users").unwrap();
        assert_eq!(*matched.value, RouteType::AppPortProxy);
        assert_eq!(matched.params.get("user_id"), Some("u6"));
        assert_eq!(matched.params.get("app_id"), Some("app-abc"));
        assert_eq!(matched.params.get("port"), Some("8080"));
        assert_eq!(matched.params.get("path"), Some("api/users"));

        // 测试端口代理路由
        let matched = router.at("/proxy/8080/api/status").unwrap();
        assert_eq!(*matched.value, RouteType::PortProxy);
        assert_eq!(matched.params.get("port"), Some("8080"));
        assert_eq!(matched.params.get("path"), Some("api/status"));

        // 测试 devapps 开发代理路由（带 path）
        let matched = router
            .at("/proxy/devapps/u6/app-abc123/4000/api/users")
            .unwrap();
        assert_eq!(*matched.value, RouteType::DevPortProxy);
        assert_eq!(matched.params.get("user_id"), Some("u6"));
        assert_eq!(matched.params.get("app_id"), Some("app-abc123"));
        assert_eq!(matched.params.get("port"), Some("4000"));
        assert_eq!(matched.params.get("path"), Some("api/users"));

        // devapps 无尾随 path 兜底
        let matched = router.at("/proxy/devapps/u6/app-abc123/4000").unwrap();
        assert_eq!(*matched.value, RouteType::DevPortProxy);
        assert_eq!(matched.params.get("port"), Some("4000"));
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
    fn test_userapp_dev_terminal_routes() {
        let router = create_router().expect("router");

        // 四协议通配 + 兜底（无尾随 path）成对
        for (path, expected) in [
            ("/proxy/userapp/ttyd/app-1/ws", RouteType::DevTtydProxy),
            ("/proxy/userapp/ttyd/app-1", RouteType::DevTtydProxy),
            ("/proxy/userapp/vnc/app-1/vnc.html", RouteType::DevVncProxy),
            ("/proxy/userapp/vnc/app-1", RouteType::DevVncProxy),
            ("/proxy/userapp/audio/app-1/ws", RouteType::DevAudioProxy),
            ("/proxy/userapp/audio/app-1", RouteType::DevAudioProxy),
            ("/proxy/userapp/ime/app-1/connect", RouteType::DevImeProxy),
            ("/proxy/userapp/ime/app-1", RouteType::DevImeProxy),
        ] {
            let matched = router.at(path).expect(path);
            assert_eq!(*matched.value, expected, "path={path}");
        }

        // 与 /proxy/devapps、/proxy/apps 互不干扰
        assert_eq!(
            *router.at("/proxy/devapps/u1/app-1/4000/x").unwrap().value,
            RouteType::DevPortProxy
        );
        assert_eq!(
            *router.at("/proxy/apps/u1/app-1/4000/x").unwrap().value,
            RouteType::AppPortProxy
        );
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
        assert_eq!(docs.len(), 11);
        // devapps 开发代理文档在列（首条为 VNC 之前插入）
        assert!(
            docs.iter()
                .any(|(p, t, _)| p.starts_with("/proxy/devapps") && t.contains("Dev apps"))
        );

        // 验证 VNC 路由文档
        assert!(docs[0].0.contains("vnc"));
        assert!(docs[0].1.contains("VNC"));

        // 验证端口代理路由文档
        assert!(docs[1].0.contains("proxy"));
        assert!(docs[1].1.contains("Port"));

        // 验证端口代理根路径兜底文档
        assert!(docs[2].0.contains("proxy"));
        assert!(docs[2].1.contains("Port"));

        // 验证健康检查路由文档
        assert!(docs[3].0.contains("health"));
        assert!(docs[3].1.contains("Health"));

        // 验证 API 代理路由文档
        assert!(docs[4].0.contains("api"));
        assert!(docs[4].1.contains("API"));

        // 验证 API 代理 fallback 路由文档
        assert!(docs[5].0.contains("api"));
        assert!(docs[5].1.contains("API"));

        // 验证 devapps 开发代理路由文档（插在 audio 之前）
        assert!(docs[6].0.contains("devapps"));
        assert!(docs[6].1.contains("Dev apps"));

        // 验证音频代理路由文档
        assert!(docs[7].0.contains("audio"));
        assert!(docs[7].1.contains("Audio"));

        // 验证 IME 代理路由文档
        assert!(docs[8].0.contains("ime"));
        assert!(docs[8].1.contains("IME"));

        // 验证 ttyd 代理路由文档
        assert!(docs[9].0.contains("ttyd"));
        assert!(docs[9].1.contains("ttyd"));

        // 验证 Web ttyd 代理路由文档
        assert!(docs[10].0.contains("web/ttyd"));
        assert!(docs[10].1.contains("Web"));
    }

    #[test]
    fn test_route_type_equality() {
        assert_eq!(RouteType::VncProxy, RouteType::VncProxy);
        assert_eq!(RouteType::PortProxy, RouteType::PortProxy);
        assert_eq!(RouteType::ApiProxy, RouteType::ApiProxy);
        assert_eq!(RouteType::HealthCheck, RouteType::HealthCheck);
        assert_eq!(RouteType::AudioProxy, RouteType::AudioProxy);
        assert_eq!(RouteType::ImeProxy, RouteType::ImeProxy);
        assert_eq!(RouteType::TtydProxy, RouteType::TtydProxy);
        assert_eq!(RouteType::WebTtydProxy, RouteType::WebTtydProxy);
        assert_ne!(RouteType::VncProxy, RouteType::PortProxy);
        assert_ne!(RouteType::ApiProxy, RouteType::PortProxy);
        assert_ne!(RouteType::AudioProxy, RouteType::ImeProxy);
        assert_ne!(RouteType::TtydProxy, RouteType::VncProxy);
        assert_ne!(RouteType::WebTtydProxy, RouteType::TtydProxy);
    }

    #[test]
    fn test_route_type_debug() {
        let vnc = RouteType::VncProxy;
        let port = RouteType::PortProxy;
        let api = RouteType::ApiProxy;
        let health = RouteType::HealthCheck;
        let audio = RouteType::AudioProxy;
        let ime = RouteType::ImeProxy;
        let ttyd = RouteType::TtydProxy;
        let web_ttyd = RouteType::WebTtydProxy;

        let vnc_str = format!("{:?}", vnc);
        let port_str = format!("{:?}", port);
        let api_str = format!("{:?}", api);
        let health_str = format!("{:?}", health);
        let audio_str = format!("{:?}", audio);
        let ime_str = format!("{:?}", ime);
        let ttyd_str = format!("{:?}", ttyd);
        let web_ttyd_str = format!("{:?}", web_ttyd);

        assert!(vnc_str.contains("VncProxy"));
        assert!(port_str.contains("PortProxy"));
        assert!(api_str.contains("ApiProxy"));
        assert!(health_str.contains("HealthCheck"));
        assert!(audio_str.contains("AudioProxy"));
        assert!(ime_str.contains("ImeProxy"));
        assert!(ttyd_str.contains("TtydProxy"));
        assert!(web_ttyd_str.contains("WebTtydProxy"));
    }

    #[test]
    fn test_audio_route_matching() {
        let router = create_router().unwrap();

        // 测试音频 WebSocket 路由
        let matched = router.at("/computer/audio/user_123/proj_456/ws").unwrap();
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
        let matched = router.at("/computer/ime/alice/myproject/connect").unwrap();
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
        let audio_matched = router.at("/computer/audio/user_123/proj_456/ws").unwrap();
        assert_eq!(*audio_matched.value, RouteType::AudioProxy);

        let ime_matched = router
            .at("/computer/ime/user_123/proj_456/connect")
            .unwrap();
        assert_eq!(*ime_matched.value, RouteType::ImeProxy);

        // 确保不同的路径参数被正确解析
        assert_eq!(audio_matched.params.get("path"), Some("ws"));
        assert_eq!(ime_matched.params.get("path"), Some("connect"));
    }

    #[test]
    fn test_ttyd_route_matching() {
        let router = create_router().unwrap();

        // WebSocket 路径
        let matched = router.at("/computer/ttyd/alice/myproject/ws").unwrap();
        assert_eq!(*matched.value, RouteType::TtydProxy);
        assert_eq!(matched.params.get("user_id"), Some("alice"));
        assert_eq!(matched.params.get("project_id"), Some("myproject"));
        assert_eq!(matched.params.get("path"), Some("ws"));

        // HTTP index.html 路径
        let matched = router
            .at("/computer/ttyd/alice/myproject/index.html")
            .unwrap();
        assert_eq!(*matched.value, RouteType::TtydProxy);
        assert_eq!(matched.params.get("path"), Some("index.html"));

        // 多级子路径（如 ws/token）
        let matched = router
            .at("/computer/ttyd/alice/myproject/ws/token")
            .unwrap();
        assert_eq!(*matched.value, RouteType::TtydProxy);
        assert_eq!(matched.params.get("path"), Some("ws/token"));
    }

    #[test]
    fn test_audio_ime_ttyd_route_not_conflict() {
        let router = create_router().unwrap();

        let audio_matched = router.at("/computer/audio/u/p/ws").unwrap();
        assert_eq!(*audio_matched.value, RouteType::AudioProxy);

        let ime_matched = router.at("/computer/ime/u/p/connect").unwrap();
        assert_eq!(*ime_matched.value, RouteType::ImeProxy);

        let ttyd_matched = router.at("/computer/ttyd/u/p/ws").unwrap();
        assert_eq!(*ttyd_matched.value, RouteType::TtydProxy);

        // 路径参数各自正确解析，互不串台
        assert_eq!(audio_matched.params.get("path"), Some("ws"));
        assert_eq!(ime_matched.params.get("path"), Some("connect"));
        assert_eq!(ttyd_matched.params.get("path"), Some("ws"));
    }

    #[test]
    fn test_web_ttyd_route_matching() {
        let router = create_router().unwrap();

        // 基本 WebSocket 路径
        let matched = router.at("/web/ttyd/user_123/proj_456/ws").unwrap();
        assert_eq!(*matched.value, RouteType::WebTtydProxy);
        assert_eq!(matched.params.get("user_id"), Some("user_123"));
        assert_eq!(matched.params.get("project_id"), Some("proj_456"));
        assert_eq!(matched.params.get("path"), Some("ws"));

        // index.html 路径
        let matched = router.at("/web/ttyd/user_123/proj_456/index.html").unwrap();
        assert_eq!(*matched.value, RouteType::WebTtydProxy);
        assert_eq!(matched.params.get("path"), Some("index.html"));

        // 多级路径（ws/token）
        let matched = router.at("/web/ttyd/user_123/proj_456/ws/token").unwrap();
        assert_eq!(*matched.value, RouteType::WebTtydProxy);
        assert_eq!(matched.params.get("path"), Some("ws/token"));
    }

    #[test]
    fn test_web_ttyd_and_computer_ttyd_not_conflict() {
        let router = create_router().unwrap();

        // 确保 web ttyd 和 computer ttyd 路由不会互相干扰
        let web_matched = router.at("/web/ttyd/u/p/ws").unwrap();
        assert_eq!(*web_matched.value, RouteType::WebTtydProxy);

        let computer_matched = router.at("/computer/ttyd/u/p/ws").unwrap();
        assert_eq!(*computer_matched.value, RouteType::TtydProxy);

        // 路径参数各自正确解析
        assert_eq!(web_matched.params.get("path"), Some("ws"));
        assert_eq!(computer_matched.params.get("path"), Some("ws"));
    }
}
