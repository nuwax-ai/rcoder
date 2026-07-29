use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrepareReleaseRequest {
    pub release_id: String,
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub retention: Option<u16>,
}

#[derive(Debug, Clone, Deserialize, Default, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActivateReleaseRequest {
    #[serde(default)]
    pub readiness_timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfirmReleaseRequest {
    pub healthy: bool,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum ReleaseStatus {
    Prepared,
    PendingStart,
    Active,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseInfo {
    pub release_id: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub status: ReleaseStatus,
    pub created_at: String,
    pub activated_at: Option<String>,
    pub failure_message: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseIndex {
    pub active_release_id: Option<String>,
    pub pending_release_id: Option<String>,
    pub previous_release_id: Option<String>,
    pub retention: u16,
    pub releases: Vec<ReleaseInfo>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseListResponse {
    pub active_release_id: Option<String>,
    pub pending_release_id: Option<String>,
    pub retention: u16,
    pub releases: Vec<ReleaseInfo>,
}
