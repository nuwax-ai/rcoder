//! Trait definitions for agent abstraction.

pub mod agent;
pub mod session_notifier;

pub use agent::*;
pub use session_notifier::{NoOpSessionNotifier, SessionNotifier};
