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
