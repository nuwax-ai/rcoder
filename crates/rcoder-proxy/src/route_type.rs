//! Pingora 代理路由类型枚举（从 router.rs 拆出——枚举+文档自成一档）。
//!
//! 定义 Pingora 代理支持的所有路由类型，用于类型安全的路由分发；
//! 路由表构建（create_router）仍在 [`crate::router`]。

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

    /// userApp 开发域终端代理族: `/userapp/{ttyd,vnc,audio,ime}/{app_id}/{*path}`
    ///
    /// 按 **app_id** 定位该 app 的 UserAppBuilder 开发容器（镜像同款：ttyd/noVNC/
    /// 音频/IME 全套），与 computer 族（user_id 定位沙箱）对称的开发场景入口。
    ///
    /// **ttyd**: `/userapp/ttyd/{app_id}/{*path}` → 容器 ws_terminal(17681) → ttyd 本体；
    ///   终端 cwd = 开发卷 `{USERAPP_WORKSPACE_ROOT}/{app_id}`（X-Ttyd-Service-Type 注入）
    /// **vnc**: `/userapp/vnc/{app_id}/{*path}` → 容器 noVNC(6080, HTTP+WS)
    /// **audio**: `/userapp/audio/{app_id}/{*path}` → ws* 6089 流 / 其余 6090 静态
    /// **ime**: `/userapp/ime/{app_id}/{*path}` → 容器 IME(6091, WebSocket)
    ///
    /// 定位走 find_by_project_id(app_id, UserAppBuilder)（注册表），miss → 404
    /// （提示先创建 workspace）；不走 vnc_backends（user_id 键空间，防撞键）。
    DevTtydProxy,
    DevVncProxy,
    DevAudioProxy,
    DevImeProxy,

    /// userApp 运行容器（部署后的生产环境）终端/数据库控制台代理族:
    /// `/userapp/{ttyd,pgweb}/{app_id}/runtime/{*path}`
    ///
    /// 按 **app_id** 定位 `ServiceType::UserApp` 运行容器（app-runtime 镜像），
    /// 与开发域四服务（无 runtime 段）对称的生产场景入口：
    /// **ttyd**: `/userapp/ttyd/{app_id}/runtime/{*path}` → 直连 ttyd 本体(7681, WS)；
    ///   运行容器无 agent_runner → 不经 ws_terminal(17681) 中间层
    /// **pgweb**: `/userapp/pgweb/{app_id}/runtime/{*path}` → 直连 pgweb(8081, HTTP)
    ///
    /// 定位走 find_app_runtime_addr（确定性命名构造——运行容器不进注册表，
    /// project_to_container[app_id] 单值键被 builder 占用）；K8s=Service FQDN
    /// （Pod 重建 DNS 自愈）。app 未部署/停止 → 上游连接失败 502。
    RuntimeTtydProxy,
    RuntimePgwebProxy,

    /// DBX 数据库 Web GUI 两阶段代理族: `/proxy/{dev,prod}/dbx/{app_id}/{*path}`
    ///
    /// dbx-web（60+ 数据库 GUI，两镜像 supervisor 恒起 :4224）按 **app_id** 定位，
    /// stage 静态段区分定位方式（与 devapps/apps 端口代理族的 dev/prod 语义对齐）：
    /// **dev**: `/proxy/dev/dbx/{app_id}/{*path}` → UserAppBuilder 开发容器
    ///   （agent-runner 镜像）；注册表 find_by_project_id(app_id, UserAppBuilder)，
    ///   未建 workspace → 404（同 dev 终端族）
    /// **prod**: `/proxy/prod/dbx/{app_id}/{*path}` → UserApp 运行容器
    ///   （app-runtime 镜像）；find_app_runtime_addr 确定性命名构造，
    ///   未部署/停止 → 上游连接失败 502（同 runtime 族）
    ///
    /// 代理剥前缀直连 root 模式 dbx（同 pgweb）：前端 webPath.ts 从
    /// location.pathname 运行时推断 base，API/WS 自动拼回 `/proxy/{stage}/dbx/{app_id}`。
    DevDbxProxy,
    ProdDbxProxy,

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
