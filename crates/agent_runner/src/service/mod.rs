pub mod agent_registry;
pub mod chat_handler;
pub mod local_agent_service;
mod session_cache;
mod session_notifier;
mod state_aware_notifier;

pub use agent_registry::{AGENT_REGISTRY, AgentSessionRegistry, PendingGuard};
pub use chat_handler::{ChatHandlerContext, ChatHandlerInput, ChatHandlerOutput, handle_chat_core};
pub use session_cache::{SESSION_CACHE, SessionData, push_session_update_with_project};
pub use state_aware_notifier::StateAwareNotifier;
