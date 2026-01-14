//! Trait definitions for agent abstraction.

pub mod agent;
pub mod session_notifier;
pub mod session_registry;

pub use agent::{AgentStartConfig, PromptMessage};
pub use session_notifier::SessionNotifier;
pub use session_registry::SessionRegistry;
