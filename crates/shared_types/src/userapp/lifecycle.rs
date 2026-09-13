//! Durable userApp control-plane contracts. Runtime identities remain authoritative
//! for physical resources; these records describe application intent and progress.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum UserAppLifecycleState {
    Active,
    Deleting,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UserAppLifecycleRecord {
    pub app_id: String,
    pub user_id: String,
    pub lifecycle_id: String,
    pub lifecycle_epoch: i64,
    pub metadata_revision: i64,
    pub state: UserAppLifecycleState,
    pub name: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub current_operation_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum UserAppOperationState {
    Pending,
    Running,
    WaitingRetry,
    RecoveryRequired,
    Succeeded,
    Failed,
}
impl UserAppOperationState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum UserAppOperationKind {
    EnsureBuilder,
    Create,
    Update,
    Start,
    Stop,
    HotDeploy,
    DeleteCompute,
    PurgeResources,
    DestroyDevStorage,
    DestroyProdStorage,
    DeleteApplication,
}
impl UserAppOperationKind {
    pub fn ends_lifecycle(self) -> bool {
        matches!(self, Self::DestroyProdStorage | Self::DeleteApplication)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct UserAppOperationRecord {
    pub operation_id: String,
    pub app_id: String,
    pub lifecycle_id: String,
    pub request_id: Option<String>,
    pub request_fingerprint: String,
    pub kind: UserAppOperationKind,
    pub state: UserAppOperationState,
    pub revision: i64,
    /// Execution claim, distinct from the public operation ID.
    #[serde(default)]
    pub executor_id: Option<String>,
    pub step: String,
    /// Non-secret runtime identities and recovery references, never credentials.
    pub checkpoint: serde_json::Value,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct UserAppAdmission {
    pub app_id: String,
    pub user_id: String,
    pub lifecycle_id: Option<String>,
    pub operation_id: String,
    pub request_id: Option<String>,
    pub request_fingerprint: String,
    pub kind: UserAppOperationKind,
}

#[derive(Debug, Clone)]
pub enum UserAppAdmissionOutcome {
    Accepted(UserAppOperationRecord),
    Existing(UserAppOperationRecord),
}

#[derive(Debug, Clone)]
pub struct UserAppOperationProgress {
    pub app_id: String,
    pub operation_id: String,
    pub lifecycle_id: String,
    pub expected_revision: i64,
    pub executor_id: String,
    pub state: UserAppOperationState,
    pub step: String,
    pub checkpoint: serde_json::Value,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

/// None preserves a field; Some(None) explicitly clears it.
#[derive(Debug, Clone)]
pub struct UserAppMetadataPatch {
    pub app_id: String,
    pub user_id: String,
    pub lifecycle_id: String,
    pub expected_revision: i64,
    pub name: Option<Option<String>>,
    pub tenant_id: Option<Option<String>>,
    pub space_id: Option<Option<String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum UserAppStoreError {
    #[error("Application ownership conflict")]
    OwnershipConflict,
    #[error("Application lifecycle conflict")]
    LifecycleConflict,
    #[error("Application operation in progress: {0}")]
    OperationInProgress(String),
    #[error("Application state version conflict")]
    VersionConflict,
    #[error("Application not found")]
    NotFound,
    #[error("Invalid application operation: {0}")]
    InvalidOperation(String),
    #[error("Application storage failed: {0}")]
    Storage(#[source] anyhow::Error),
}

/// All mutating methods commit atomically and return only after commit. A storage
/// error is never equivalent to absence. Implementations must not call runtimes.
#[async_trait::async_trait]
pub trait UserAppLifecycleStore: Send + Sync {
    async fn ensure_identity(
        &self,
        app_id: &str,
        user_id: &str,
    ) -> Result<UserAppLifecycleRecord, UserAppStoreError>;
    async fn get_application(
        &self,
        app_id: &str,
    ) -> Result<Option<UserAppLifecycleRecord>, UserAppStoreError>;
    async fn patch_metadata(
        &self,
        patch: &UserAppMetadataPatch,
    ) -> Result<UserAppLifecycleRecord, UserAppStoreError>;
    async fn admit(
        &self,
        request: &UserAppAdmission,
    ) -> Result<UserAppAdmissionOutcome, UserAppStoreError>;
    async fn advance(
        &self,
        progress: &UserAppOperationProgress,
    ) -> Result<UserAppOperationRecord, UserAppStoreError>;
    async fn get_operation(
        &self,
        app_id: &str,
        operation_id: &str,
    ) -> Result<Option<UserAppOperationRecord>, UserAppStoreError>;
    async fn unfinished_operations(
        &self,
        limit: u32,
    ) -> Result<Vec<UserAppOperationRecord>, UserAppStoreError>;
    async fn recreate(
        &self,
        app_id: &str,
        user_id: &str,
        expected_lifecycle_id: &str,
        request_id: &str,
    ) -> Result<UserAppLifecycleRecord, UserAppStoreError>;
}
