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

// === gRPC 超时配置 ===

/// gRPC 连接超时（秒）
pub const GRPC_CONNECT_TIMEOUT_SECS: u64 = 5;

/// gRPC 请求超时（秒）
///
/// Chat 等可能较慢的请求需要较长超时
pub const GRPC_REQUEST_TIMEOUT_SECS: u64 = 300;

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
/// - 足够处理突发请求（100 个）
/// - 通道满时异步等待，防止 OOM
/// - 可通过环境变量 AGENT_PROMPT_CHANNEL_CAPACITY 覆盖
pub const AGENT_PROMPT_CHANNEL_CAPACITY: usize = 100;

/// Agent 取消通道容量
///
/// 控制 Agent 取消请求队列的大小
/// - 取消请求通常较少，使用相同容量保持一致性
/// - 可通过环境变量 AGENT_CANCEL_CHANNEL_CAPACITY 覆盖
pub const AGENT_CANCEL_CHANNEL_CAPACITY: usize = 100;
