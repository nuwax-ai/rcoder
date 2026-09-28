pub mod agent_registry;
pub mod agent_session_service;
mod cancel;
pub mod chat_handler;
#[cfg(feature = "http-server")]
pub mod local_agent_service;
mod logging_diagnostics_listener;
pub mod permission_manager;
mod session_cache;
mod session_notifier;
mod state_aware_notifier;

pub use agent_registry::{AGENT_REGISTRY, AgentSessionRegistry, PendingGuard};
pub use agent_session_service::{AgentRequest, AgentSessionService};
#[allow(unused_imports)]
pub use chat_handler::{ChatHandlerContext, ChatHandlerInput, handle_chat_core};
pub use logging_diagnostics_listener::LoggingDiagnosticsListener;
pub use permission_manager::PERMISSION_MANAGER;
/// 会话 ring buffer 生产容量（仅 crate 内使用，不进公共 API）。
pub(crate) use session_cache::RING_BUFFER_SIZE;
pub use session_cache::{SESSION_CACHE, SessionData, push_session_update_with_project};
pub use state_aware_notifier::StateAwareNotifier;
