//! 全局常量定义
//!
//! 集中管理所有服务端口、超时等配置常量

// === 端口配置 ===

/// gRPC 服务默认端口
///
/// agent_runner gRPC 服务监听端口
pub const GRPC_DEFAULT_PORT: u16 = 50051;

/// HTTP 服务默认端口
///
/// agent_runner HTTP 服务（健康检查等）端口
pub const HTTP_DEFAULT_PORT: u16 = 8086;

/// noVNC 服务端口
///
/// agent_runner noVNC 服务端口（Web VNC 访问）
pub const NOVNC_PORT: u16 = 6080;

/// Xvnc RFB 后端端口
///
/// 容器内 Xvnc 进程监听的 RFB（Remote Frame Buffer）端口，websockify
/// （NOVNC_PORT=6080）代理到此端口。VNC 状态探测必须读到该端口返回的
/// RFB 协议版本串（`RFB 003.00x\n`）才能证明 Xvnc 真在处理连接——
/// 仅 TCP connect 成功（端口 listen）不能排除 Xvnc 卡死/僵尸。
pub const XVNC_RFB_PORT: u16 = 5900;

/// ttyd 本体端口（恒为 7681）
///
/// 容器内 ttyd 进程监听端口。computer/web 两种 ttyd 场景下，agent_runner 的 ws 中间层
/// （`WS_TERMINAL_PORT`=17681）都用此常量 connect 本地 ttyd；ttyd 本身不再对外，
/// Pingora 只连 ws 中间层（17681）。
pub const TTYD_PORT: u16 = 7681;

/// agent_runner WS 终端中间层端口（computer ttyd 场景）
///
/// agent_runner 用 tokio-tungstenite 在浏览器和本地 ttyd 之间做 WS 中间控制层，监听此端口；
/// Pingora 的 TtydProxy（/computer/ttyd/*）路由到此端口。ttyd 本体仍在 TTYD_PORT（7681），
/// 仅 agent_runner 内部连接，不对外暴露（K8s Service 暴露此 17681 而非 ttyd 7681）。
pub const WS_TERMINAL_PORT: u16 = 17681;

// === K8s 配置 ===

/// K8s 集群域名环境变量名
pub const K8S_CLUSTER_DOMAIN_ENV: &str = "RCODER_K8S_CLUSTER_DOMAIN";

/// K8s 默认集群域名
pub const K8S_DEFAULT_CLUSTER_DOMAIN: &str = "cluster.local";

/// 获取 K8s 集群域名
///
/// 从环境变量 `RCODER_K8S_CLUSTER_DOMAIN` 获取，如果未设置则返回默认值 "cluster.local"
pub fn get_k8s_cluster_domain() -> String {
    std::env::var(K8S_CLUSTER_DOMAIN_ENV).unwrap_or_else(|_| K8S_DEFAULT_CLUSTER_DOMAIN.to_string())
}

/// 判断是否是 K8s 运行时（通过 features flag）
///
/// 返回 `true` 表示当前是 K8s 运行时，`false` 表示 Docker 运行时
pub fn is_kubernetes_runtime() -> bool {
    cfg!(feature = "kubernetes")
}

/// 构建 K8s Service FQDN
///
/// # 参数
/// - `container_name`: 容器名称
/// - `namespace`: K8s namespace
/// - `cluster_domain`: K8s 集群域名
///
/// # 返回
/// K8s Service FQDN，格式如 `container-svc.namespace.svc.cluster.local`
pub fn build_k8s_service_fqdn(
    container_name: &str,
    namespace: &str,
    cluster_domain: &str,
) -> String {
    format!(
        "{}-svc.{}.svc.{}",
        container_name, namespace, cluster_domain
    )
}

/// 构建后端地址
///
/// 根据运行环境自动选择后端地址：
/// - K8s 环境：使用 K8s Service FQDN（格式：`container-svc.namespace.svc.cluster.local`）
/// - Docker 环境：使用容器 IP（格式：`192.168.1.100`）
///
/// # 参数
/// - `container_name`: 容器名称（用于 K8s Service FQDN 构建）
/// - `container_ip`: 容器 IP（Docker 环境使用）
/// - `namespace`: K8s namespace
/// - `cluster_domain`: K8s 集群域名
///
/// # 返回
/// 后端地址，格式如 `host`（不含端口）
pub fn build_backend_addr(
    container_name: &str,
    container_ip: &str,
    namespace: &str,
    cluster_domain: &str,
) -> String {
    if is_kubernetes_runtime() {
        // K8s 环境：使用 K8s Service FQDN
        build_k8s_service_fqdn(container_name, namespace, cluster_domain)
    } else {
        // Docker 环境：使用容器 IP
        container_ip.to_string()
    }
}

/// 构建 gRPC 地址
///
/// 根据运行环境自动选择 gRPC 地址：
/// - K8s 环境：使用 K8s Service FQDN（格式：`container-svc.namespace.svc.cluster.local:50051`）
/// - Docker 环境：使用容器 IP（格式：`192.168.1.100:50051`）
///
/// # 参数
/// - `container_name`: 容器名称（用于 K8s Service FQDN 构建）
/// - `container_ip`: 容器 IP（Docker 环境使用）
/// - `namespace`: K8s namespace
/// - `cluster_domain`: K8s 集群域名
///
/// # 返回
/// gRPC 地址，格式如 `host:port`
pub fn build_grpc_addr(
    container_name: &str,
    container_ip: &str,
    namespace: &str,
    cluster_domain: &str,
) -> String {
    let backend_addr = build_backend_addr(container_name, container_ip, namespace, cluster_domain);
    format!("{}:{}", backend_addr, GRPC_DEFAULT_PORT)
}

/// 构建 HTTP 地址
///
/// 根据运行环境自动选择 HTTP 地址：
/// - K8s 环境：使用 K8s Service FQDN（格式：`container-svc.namespace.svc.cluster.local:8086`）
/// - Docker 环境：使用容器 IP（格式：`192.168.1.100:8086`）
///
/// # 参数
/// - `container_name`: 容器名称（用于 K8s Service FQDN 构建）
/// - `container_ip`: 容器 IP（Docker 环境使用）
/// - `namespace`: K8s namespace
/// - `cluster_domain`: K8s 集群域名
///
/// # 返回
/// HTTP 地址，格式如 `host:port`
pub fn build_http_addr(
    container_name: &str,
    container_ip: &str,
    namespace: &str,
    cluster_domain: &str,
) -> String {
    let backend_addr = build_backend_addr(container_name, container_ip, namespace, cluster_domain);
    format!("{}:{}", backend_addr, HTTP_DEFAULT_PORT)
}

// === gRPC 超时配置 ===

/// gRPC 连接超时（秒）
pub const GRPC_CONNECT_TIMEOUT_SECS: u64 = 5;

/// gRPC 消息大小限制（字节）
///
/// 设置为 128MB（tonic 默认 4MB 不够大）
/// 用于处理大型文件传输（如截图、PDF 等）
///
/// 注意：客户端和服务端都需要配置此限制
pub const GRPC_MAX_MESSAGE_SIZE: usize = 128 * 1024 * 1024; // 128MB

/// gRPC 请求超时（秒）
///
/// 用于 Channel 级别的兜底超时（连接池里的所有请求）。设为 5 分钟避免
/// 极端慢请求被误杀；具体 RPC 的超时由各 handler 单独控制。
pub const GRPC_REQUEST_TIMEOUT_SECS: u64 = 300;

/// gRPC Chat RPC 超时（秒）
///
/// Chat RPC 实际是"启动 + 接受"语义：agent_runner 收到请求后会先确保 agent 进程
/// 就绪（某些 agent 如 codex-acp 启动慢，需要加载配置、初始化上下文服务等），
/// 然后才返回 session_id。所以这个超时必须覆盖 agent 启动时间。
///
/// 历史值：300s（5 分钟）—— 太长，agent_runner 卡死时客户端会傻等
/// 当前值：120s（2 分钟）—— 平衡：覆盖慢启动 agent，又能及时检测真正卡死
///
/// 进度推送不依赖这个超时，走 SubscribeProgress 流（长连接，独立超时）。
pub const GRPC_CHAT_TIMEOUT_SECS: u64 = 120;

/// CancelSession 请求超时（秒）
///
/// 取消会话应该是快速操作（仅发送取消信号），10 秒足够
pub const GRPC_CANCEL_SESSION_TIMEOUT_SECS: u64 = 10;

/// StopAgent 请求超时（秒）
///
/// 停止 Agent 需要等待当前操作完成和进程退出，30 秒合理
pub const GRPC_STOP_AGENT_TIMEOUT_SECS: u64 = 30;

/// ResolvePermission 请求超时（秒）
///
/// 权限解析是简单的状态更新操作，10 秒足够
pub const GRPC_RESOLVE_PERMISSION_TIMEOUT_SECS: u64 = 10;

// === SSE 配置 ===

/// SSE Keep-alive 间隔（秒）
pub const SSE_KEEPALIVE_INTERVAL_SECS: u64 = 15;

// === Session 配置 ===

/// Session 等待超时（秒）
///
/// 等待 session 在缓存中出现的最大时间
pub const SESSION_WAIT_TIMEOUT_SECS: u64 = 30;

/// Session 消息缓冲区大小
pub const SESSION_MESSAGE_BUFFER_SIZE: usize = 100;

// === Agent 通道配置 ===

/// Agent Prompt 通道容量
///
/// 控制 Agent Prompt 请求队列的大小，提供背压保护
/// - 足够处理突发请求（1000 个）
/// - 通道满时异步等待，防止 OOM
/// - 可通过环境变量 AGENT_PROMPT_CHANNEL_CAPACITY 覆盖
///
/// ## P2 优化
///
/// 从 100 增加到 1000，以更好地处理高并发场景。
pub const AGENT_PROMPT_CHANNEL_CAPACITY: usize = 1000;

/// Agent 取消通道容量
///
/// 控制 Agent 取消请求队列的大小
/// - 取消请求通常较少，使用相同容量保持一致性
/// - 可通过环境变量 AGENT_CANCEL_CHANNEL_CAPACITY 覆盖
///
/// ## P2 优化
///
/// 从 100 增加到 1000，与 Prompt 通道保持一致。
pub const AGENT_CANCEL_CHANNEL_CAPACITY: usize = 1000;

// === 内置 ACP Agent ===

/// 默认 ACP agent 标识符
///
/// 当请求未指定 agent_id 时，使用此默认值。
pub const DEFAULT_AGENT_ID: &str = "claude-code-acp-ts";

/// 内置 ACP agent 标识符列表
///
/// 容器构建时已预装，chat 接口跳过自动安装逻辑。
pub const BUILTIN_AGENT_IDS: &[&str] = &[DEFAULT_AGENT_ID, "nuwaxcode"];

/// 判断是否为内置 agent
pub fn is_builtin_agent(agent_id: &str) -> bool {
    BUILTIN_AGENT_IDS.contains(&agent_id)
}
