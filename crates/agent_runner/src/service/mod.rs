mod agent_registry;
mod session_cache;
mod session_notifier;
mod state_aware_notifier;

pub use agent_registry::{AgentSessionRegistry, RegistryStats, AGENT_REGISTRY};
pub use session_cache::{
    ensure_project_session, push_session_update, push_session_update_with_project,
    SessionData, SESSION_CACHE,
};
pub use state_aware_notifier::StateAwareNotifier;
