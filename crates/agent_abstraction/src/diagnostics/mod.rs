//! Process diagnostics module — exposes agent subprocess lifecycle information.
//!
//! Provides [`ProcessDiagnostics`] (structured information about an agent process)
//! and [`DiagnosticsListener`] (callback trait for receiving diagnostic events).
//!
//! This module is primarily consumed by CLI tools that need detailed error output
//! when agent startup fails. The `agent_runner` service typically does not inject
//! a listener, so the diagnostics overhead is zero.

mod listener;
mod types;

pub use listener::{DiagnosticsListener, NoopDiagnosticsListener};
pub use types::ProcessDiagnostics;
