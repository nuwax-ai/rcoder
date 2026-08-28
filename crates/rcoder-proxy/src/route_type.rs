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

    /// userApp 生产应用流量代理（免端口）: `/proxy/userapp/prod/{user_id}/{app_id}/{*path}`
    ///
    /// - `user_id`: 用户 ID（不参与解析；日志排障/归属鉴权锚点）
    /// - `app_id`: 应用 ID（`app_backends` 注册表定位 UserApp 运行容器）
    /// - `path`: 剩余路径
    ///
    /// **目标**: app_manager 部署的运行容器，内部固定拨 pingap 统一入口
    /// `APP_ENTRY_PORT`(9080)——调用方无需传端口；未注册 9080 且该 app 恰只有
    /// 一个已注册 HTTP 端口时回退用之（防御直接 REST create 自定义端口），否则 502。
    /// request_filter 对本路由做访问追踪 + 停止唤醒。
    ///
    /// **示例**: `/proxy/userapp/prod/6/app-1a2b3c4d/api/users`
    ProdAppProxy,

    /// userApp 开发应用流量代理（免端口）: `/proxy/userapp/dev/{user_id}/{app_id}/{*path}`
    ///
    /// - `user_id`: 用户 ID（不参与解析；日志排障/归属鉴权锚点）
    /// - `app_id`: 应用 ID（动态解析该 app 的开发容器（UserAppBuilder，per-app））
    /// - `path`: 剩余路径
    ///
    /// **目标**: 该 app 开发容器的 pingap 统一入口 `APP_ENTRY_PORT`(9080)——
    /// manifest 流程恒定 9080，调用方无需传端口；与 ProdAppProxy（运行容器）
    /// 对称的开发预览入口，切环境只改 `dev→prod` 一段。
    ///
    /// **示例**: `/proxy/userapp/dev/6/app-1a2b3c4d/api/users`
    DevAppProxy,

    /// userApp 开发域工具代理族: `/userapp/dev/{ttyd,vnc,audio,ime,dbx}/{app_id}/{*path}`
    ///
    /// 按 **app_id** 定位该 app 的 UserAppBuilder 开发容器（镜像同款：ttyd/noVNC/
    /// 音频/IME/DBX 全套），与 computer 族（user_id 定位沙箱）对称的开发场景入口；
    /// stage 段 `dev` 与 prod 工具族/流量族 `/proxy/userapp/{dev,prod}` 语义统一。
    ///
    /// **ttyd**: `/userapp/dev/ttyd/{app_id}/{*path}` → 容器 ws_terminal(17681) → ttyd 本体；
    ///   终端 cwd = 开发卷 `{USERAPP_WORKSPACE_ROOT}/{app_id}`（X-Ttyd-Service-Type 注入）
    /// **vnc**: `/userapp/dev/vnc/{app_id}/{*path}` → 容器 noVNC(6080, HTTP+WS)
    /// **audio**: `/userapp/dev/audio/{app_id}/{*path}` → ws* 6089 流 / 其余 6090 静态
    /// **ime**: `/userapp/dev/ime/{app_id}/{*path}` → 容器 IME(6091, WebSocket)
    ///
    /// 定位走 find_by_project_id(app_id, UserAppBuilder)（注册表），miss → 404
    /// （提示先创建 workspace）；不走 vnc_backends（user_id 键空间，防撞键）。
    DevTtydProxy,
    DevVncProxy,
    DevAudioProxy,
    DevImeProxy,

    /// userApp 生产域工具代理族（运行容器，部署后的生产环境）:
    /// `/userapp/prod/{ttyd,pgweb,dbx}/{app_id}/{*path}`
    ///
    /// 按 **app_id** 定位 `ServiceType::UserApp` 运行容器（app-runtime 镜像），
    /// 与开发域工具族对称的生产场景入口（stage 段 `prod`，原 `/runtime` 静态段退役）：
    /// **ttyd**: `/userapp/prod/ttyd/{app_id}/{*path}` → 直连 ttyd 本体(7681, WS)；
    ///   运行容器无 agent_runner → 不经 ws_terminal(17681) 中间层
    /// **pgweb**: `/userapp/prod/pgweb/{app_id}/{*path}` → 直连 pgweb(8081, HTTP)
    ///
    /// 定位走 find_app_runtime_addr（确定性命名构造——运行容器不进注册表，
    /// project_to_container[app_id] 单值键被 builder 占用）；K8s=Service FQDN
    /// （Pod 重建 DNS 自愈）。app 未部署/停止 → 上游连接失败 502。
    RuntimeTtydProxy,
    RuntimePgwebProxy,

    /// DBX 数据库 Web GUI 两阶段代理族: `/userapp/{dev,prod}/dbx/{user_id}/{app_id}/{*path}`
    ///
    /// dbx-web（60+ 数据库 GUI，两镜像 supervisor 恒起 :4224）按 **app_id** 定位，
    /// **user_id** 是 dev 懒创建显式 owner 档（`dev/{user_id}/{app_id}` 宿主树分区）
    /// 与 prod 归属校验锚点；stage 段区分定位方式（归入工具族 stage 语义）：
    /// **dev**: `/userapp/dev/dbx/{user_id}/{app_id}/{*path}` → UserAppBuilder 开发容器
    ///   （agent-runner 镜像）；注册表 find_by_project_id(app_id, UserAppBuilder)，
    ///   未建 workspace → 404（同 dev 工具族）
    /// **prod**: `/userapp/prod/dbx/{user_id}/{app_id}/{*path}` → UserApp 运行容器
    ///   （app-runtime 镜像）；find_app_runtime_addr 确定性命名构造，
    ///   未部署/停止 → 唤醒（wake-without-touch）或上游失败 502（同 prod 工具族）
    ///
    /// 代理剥前缀直连 root 模式 dbx（同 pgweb）：前端 webPath.ts 从
    /// location.pathname 运行时推断 base，API/WS 自动拼回 `/userapp/{stage}/dbx/{user_id}/{app_id}`。
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
