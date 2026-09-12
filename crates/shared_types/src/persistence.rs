//! Persistence lifecycle and shutdown contracts shared by the platform and stores.

/// A flush result describes durability, independently from task termination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlushOutcome {
    Complete,
    Incomplete { pending: usize, reason: String },
    TimedOut { pending: usize },
}

impl FlushOutcome {
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// Opaque lifecycle identities survive hydration and never derive from timestamps.
#[derive(Debug, Clone)]
pub struct ProjectPersistenceIdentity {
    pub generation: String,
    pub predecessor: Option<String>,
    pub sessions: std::collections::BTreeMap<String, String>,
    pub retired_sessions: std::collections::BTreeMap<String, String>,
}

impl Default for ProjectPersistenceIdentity {
    fn default() -> Self {
        Self {
            generation: uuid::Uuid::new_v4().to_string(),
            predecessor: None,
            sessions: std::collections::BTreeMap::new(),
            retired_sessions: std::collections::BTreeMap::new(),
        }
    }
}

/// Conditional persistence writes distinguish applied mutations from retired identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistenceOperationOutcome {
    Committed,
    Superseded,
}

/// Request-level outcome; deferred operations remain registered for retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistenceWriteOutcome {
    Committed,
    Superseded,
    Deferred { reason: String },
}
