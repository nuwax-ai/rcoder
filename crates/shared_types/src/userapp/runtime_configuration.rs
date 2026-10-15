//! Historical private PG configuration storage contracts. Ordinary lifecycle
//! admission does not capture these values; password changes use database administration.
use super::{
    db_admin::StartPgCredential,
    lifecycle::{UserAppExecutionContext, UserAppOperationScope, UserAppStoreError},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub const APP_RUNTIME_CONFIGURATION_VERSION: &str = "APP_RUNTIME_CONFIGURATION_VERSION";

/// Retired protocol key retained only in the reserved environment-key list.
pub const APP_RUNTIME_GENERATION_HANDOFF: &str = "APP_RUNTIME_GENERATION_HANDOFF";

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SaveRuntimeConfigurationRequest {
    pub lifecycle_id: String,
    /// Stable save identity; retries cannot replace a newer saved version.
    pub request_id: String,
    /// Zero only for the first save in this lifecycle and scope. This revision
    /// counts configuration edits, not asynchronous startup state changes.
    pub expected_revision: i64,
    pub pg: StartPgCredential,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RuntimeConfigurationStatus {
    pub lifecycle_id: String,
    /// Scope: Dev, Prod, or Application. The configuration API currently exposes Prod.
    pub scope: UserAppOperationScope,
    pub revision: i64,
    /// Latest saved version, selected by the next explicit start/restart/deploy.
    pub saved_version: i64,
    /// Database credentials confirmed applied, even if business startup failed.
    pub applied_version: Option<i64>,
    pub applying_version: Option<i64>,
    pub applying_operation_id: Option<String>,
    pub pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SavedRuntimeConfiguration {
    /// Version created by this request, which can differ from status.saved_version
    /// when a retry is observed after a later save. Never silently re-promote it.
    pub config_version: i64,
    pub status: RuntimeConfigurationStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RuntimeConfigurationTarget {
    /// Physical pod UID or container ID; never its reusable name/IP.
    pub physical_uid: String,
    pub deployment_generation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CredentialApplicationState {
    Captured,
    Applying,
    Applied,
    Failed,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BusinessStartupState {
    NotStarted,
    Starting,
    Ready,
    Failed,
    Unknown,
}

/// Internal execution payload. No Serialize/ToSchema: public operation/status
/// responses must not acquire a route to the private credentials through this type.
#[derive(Debug, Clone)]
pub struct RuntimeConfigurationCapture {
    pub operation_id: String,
    pub lifecycle_id: String,
    pub scope: UserAppOperationScope,
    pub config_version: i64,
    pub pg: StartPgCredential,
    pub target: Option<RuntimeConfigurationTarget>,
    pub credentials: CredentialApplicationState,
    pub business: BusinessStartupState,
}

#[async_trait::async_trait]
pub trait UserAppRuntimeConfigurationStore: Send + Sync {
    /// Validate current lifecycle, idempotency and revision, then save only.
    /// Must not wake a container, change PG credentials or perform network I/O.
    async fn save_runtime_configuration(
        &self,
        app_id: &str,
        scope: UserAppOperationScope,
        request: &SaveRuntimeConfigurationRequest,
    ) -> Result<SavedRuntimeConfiguration, UserAppStoreError>;
    async fn runtime_configuration_status(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        scope: UserAppOperationScope,
    ) -> Result<Option<RuntimeConfigurationStatus>, UserAppStoreError>;
    /// Exact current executor only. The version was captured atomically at
    /// admission; reading this method must never choose a newer pending version.
    async fn operation_runtime_configuration(
        &self,
        context: &UserAppExecutionContext,
    ) -> Result<Option<RuntimeConfigurationCapture>, UserAppStoreError>;
    /// Bind once before changing PG. A different physical generation requires a
    /// new explicit operation, not overwriting an uncertain operation's receipt.
    async fn bind_runtime_configuration_target(
        &self,
        context: &UserAppExecutionContext,
        config_version: i64,
        target: &RuntimeConfigurationTarget,
    ) -> Result<(), UserAppStoreError>;
    /// Credentials and service readiness are separate durable facts. Unknown is
    /// not failure evidence and cannot release the lifecycle operation or lease.
    /// Applying -> Failed requires confirmed NotAttempted mutation evidence from
    /// the executor. An interrupted or unacknowledged write must use Unknown.
    async fn record_runtime_configuration_result(
        &self,
        context: &UserAppExecutionContext,
        config_version: i64,
        target: &RuntimeConfigurationTarget,
        credentials: CredentialApplicationState,
        business: BusinessStartupState,
    ) -> Result<(), UserAppStoreError>;
}
