//! Evidence that a runtime request was explicitly rejected, not a transport timeout.
#[derive(Debug, thiserror::Error)]
#[error("{message} (HTTP {status})")]
pub struct RuntimeRequestRejection {
    pub status: u16,
    pub message: String,
}

impl RuntimeRequestRejection {
    pub fn from_status(status: u16, message: String) -> Option<Self> {
        // Request timeout and client-closed responses do not prove quiescence.
        ((400..500).contains(&status) && !matches!(status, 408 | 499))
            .then_some(Self { status, message })
    }
}
