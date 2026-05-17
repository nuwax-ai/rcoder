//! HTTP 路由和处理器模块
mod agent_cancel_handler;
mod agent_session_notification;
mod agent_status_handler;
mod agent_stop_handler;
mod chat_handler;
mod computer_agent_status_handler;
mod computer_agent_stop_handler;
mod computer_chat_handler;
mod computer_desktop_handler;
mod health_handler;
mod permission_handler;
pub mod pod_handler;
pub mod proxy_api;
pub mod proxy_handler_api;
pub mod utils;

// 调试处理器（仅在启用 debug feature 时可用）
#[cfg(feature = "debug")]
mod debug_handler;

pub use agent_cancel_handler::*;
pub use agent_session_notification::*;
pub use agent_status_handler::*;
pub use agent_stop_handler::*;
pub use chat_handler::*;
pub use computer_agent_status_handler::*;
pub use computer_agent_stop_handler::*;
pub use computer_chat_handler::*;
pub use computer_desktop_handler::*;
pub use health_handler::*;
pub use permission_handler::*;
pub use pod_handler::*;
pub use proxy_api::*;
pub use proxy_handler_api::*;

// 仅在启用 debug feature 时导出 debug handler
#[cfg(feature = "debug")]
pub use debug_handler::*;
