//! agent_runner 库
//!
//! 提供 AI 代理运行时和 ACP 协议集成

// 单树化：全部模块在 lib 树唯一声明（main.rs 只做编排入口经 `agent_runner::`）。
pub mod agent_mgmt;
pub mod api_key_manager;
pub mod auto_reload;
pub mod config;
/// tokio-console 观测（`console` feature 专用装配）
#[cfg(feature = "console")]
pub mod console_obs;
/// 内嵌 file-server（env RCODER_EMBED_FILE_SERVER 运行时开关，编译期无门控）
pub mod file_server_embed;
pub mod grpc;
pub mod handler;
pub mod model;
pub mod otel_tracing; // 🔥 设为 public，供其他模块使用
/// Pyroscope Profiler（`pyroscope` feature 专用）
#[cfg(feature = "pyroscope")]
pub mod profiler;
pub mod proxy_agent;
pub mod router;
pub mod service; // 🔥 设为 public，供测试使用
pub mod shutdown;
pub mod utils;

// 条件性编译：HTTP 服务器模块
#[cfg(feature = "http-server")]
pub mod http_server;

// ttyd WebSocket 终端中间层（接浏览器 + 连本地 ttyd，代码控制 cd）
pub mod ws_terminal;

// VNC 桌面连接活跃度计数（读 /proc/net/tcp 数 noVNC 端口 ESTABLISHED 连接，
// 供 get_active_tasks_count 折入 active_tasks，使「桌面开着」的容器不被闲置回收）
pub mod vnc_activity;

// 测试辅助模块 (仅在 testing feature 启用时编译)
#[cfg(feature = "testing")]
pub mod testing;

// 重新导出主要的类型和函数
pub use config::*;
pub use model::*;
pub use otel_tracing::*;
pub use proxy_agent::*;
pub use service::*; // 重新导出 service 模块
pub use utils::*;

#[cfg(feature = "http-server")]
pub use http_server::start::HttpServerHandle;
#[cfg(feature = "http-server")]
pub use http_server::{HttpServerConfig, start_http_server};
