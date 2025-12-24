//! Trait definitions for agent abstraction.

pub mod agent;
pub mod session_notifier;
pub mod session_registry;

pub use agent::*;
pub use session_notifier::{NoOpSessionNotifier, SessionNotifier};
pub use session_registry::SessionRegistry;
