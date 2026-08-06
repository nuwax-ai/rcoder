//! HTTP 路由和处理器模块
mod agent_cancel_handler;
pub mod agent_install_strategy;
pub mod agent_mgmt_handler;
mod agent_session_notification;
mod agent_status_handler;
mod agent_stop_handler;
pub(crate) mod chat_forward;
mod chat_handler;
mod computer_agent_status_handler;
mod computer_agent_stop_handler;
mod computer_chat_handler;
mod computer_db_handler;
mod computer_desktop_handler;
mod devcomputer_handler;
mod docs;
mod health_handler;
mod internal_handler;
mod permission_handler;
pub mod pod_handler;
pub mod proxy_api;
pub mod proxy_handler_api;
mod sse_builder;
pub mod utils;

// 调试处理器（仅在启用 debug feature 时可用）
#[cfg(feature = "debug")]
mod debug_handler;

pub use agent_cancel_handler::*;
pub use agent_mgmt_handler::*;
pub use agent_session_notification::*;
pub use agent_status_handler::*;
pub use agent_stop_handler::*;
pub use chat_handler::*;
pub use computer_agent_status_handler::*;
pub use computer_agent_stop_handler::*;
pub use computer_chat_handler::*;
pub use computer_db_handler::*;
pub use computer_desktop_handler::*;
pub use devcomputer_handler::*;
pub use docs::*;
pub use health_handler::*;
pub use internal_handler::*;
pub use permission_handler::*;
pub use pod_handler::*;
pub use proxy_api::*;
pub use proxy_handler_api::*;

// 仅在启用 debug feature 时导出 debug handler
#[cfg(feature = "debug")]
pub use debug_handler::*;
