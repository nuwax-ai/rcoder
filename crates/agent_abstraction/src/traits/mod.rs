//! Trait definitions for agent abstraction.

pub mod agent;
pub mod permission_handler;
pub mod session_notifier;
pub mod session_registry;

pub use agent::{AgentStartConfig, PromptMessage};
pub use permission_handler::{
    PermissionRequestContext, PermissionRequestHandler, YoloPermissionRequestHandler,
};
pub use session_notifier::SessionNotifier;
pub use session_registry::SessionRegistry;
