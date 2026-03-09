pub mod agent_registry;
pub mod chat_handler;
mod session_cache;
mod session_notifier;
mod state_aware_notifier;

pub use agent_registry::{AgentSessionRegistry, AGENT_REGISTRY, PendingGuard};
pub use chat_handler::{handle_chat_core, ChatHandlerContext, ChatHandlerInput, ChatHandlerOutput};
pub use session_cache::{push_session_update_with_project, SessionData, SESSION_CACHE};
pub use state_aware_notifier::StateAwareNotifier;
