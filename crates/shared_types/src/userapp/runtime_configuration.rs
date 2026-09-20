//! Private, versioned PG runtime configuration. Saving never changes a running
//! database. Admission captures a version in the same lifecycle transaction.
use super::{
    db_admin::StartPgCredential,
    lifecycle::{UserAppExecutionContext, UserAppOperationScope, UserAppStoreError},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub const APP_RUNTIME_CONFIGURATION_VERSION: &str = "APP_RUNTIME_CONFIGURATION_VERSION";

/// Trusted controller authorization, bound to the old physical replacement target.
/// The artifact is read only after the new owner has acquired exclusive ownership.
pub const APP_RUNTIME_GENERATION_HANDOFF: &str = "APP_RUNTIME_GENERATION_HANDOFF";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RuntimeGenerationHandoff {
    pub protocol_version: u32,
    pub app_id: String,
    pub lifecycle_id: String,
    pub previous_generation: String,
    pub previous_resource_uid: String,
    pub previous_resource_name: String,
    pub activation: RuntimeConfigurationActivation,
}

impl RuntimeGenerationHandoff {
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol_version != 1
            || self.activation.config_version <= 0
            || [
                &self.app_id,
                &self.lifecycle_id,
                &self.previous_generation,
                &self.previous_resource_uid,
                &self.previous_resource_name,
                &self.activation.operation_id,
                &self.activation.deployment_generation,
            ]
            .iter()
            .any(|value| value.trim().is_empty())
            || self.previous_generation == self.activation.deployment_generation
            || self.activation.operation_id != self.activation.deployment_generation
        {
            return Err("Invalid runtime generation handoff authorization".into());
        }
        Ok(())
    }
}

/// Durable reservation produced by the source owner before physical replacement.
/// It freezes new runtime mutations but does not authorize credential changes.
/// The destination must still establish its own RuntimeGenerationPrepared receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RuntimeGenerationSourceSeal {
    pub authorization: RuntimeGenerationHandoff,
    pub artifact_release_id: String,
    pub source_journal_sha256: String,
    pub desired_revision: u64,
}

/// Durable evidence produced under the new owner's exclusive workspace lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RuntimeGenerationPrepared {
    pub authorization: RuntimeGenerationHandoff,
    pub artifact_release_id: String,
    pub release_manifest_sha256: String,
    pub previous_journal_sha256: String,
    pub execution_workspace: String,
    /// Desired revision when the fresh explicit operation was accepted. Later
    /// Stop revisions invalidate this intent even when container env is unchanged.
    pub desired_revision: u64,
}

/// Platform acknowledgement after durable credential application. Contains no
/// credentials. It only opens the captured generation's business startup gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfigurationActivation {
    pub operation_id: String,
    pub deployment_generation: String,
    pub config_version: i64,
}

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
