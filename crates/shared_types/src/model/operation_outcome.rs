//! An admitted execution lost its result; callers must query its original identity.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{detail}")]
pub struct OperationOutcomeUnknown {
    pub stage: &'static str,
    pub detail: String,
}

impl OperationOutcomeUnknown {
    pub fn new(stage: &'static str, detail: impl Into<String>) -> Self {
        Self {
            stage,
            detail: detail.into(),
        }
    }
}
