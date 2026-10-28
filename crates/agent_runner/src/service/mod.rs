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
pub use session_cache::{SESSION_CACHE, SessionData, push_session_update_with_project};
pub(crate) use session_cache::{get_or_create_session_data, require_cache_session_id};
pub use state_aware_notifier::StateAwareNotifier;
