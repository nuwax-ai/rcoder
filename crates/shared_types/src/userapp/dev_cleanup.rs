//! Identity-bound UserApp builder deletion contracts.
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderDeletionSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_binding: Option<crate::UserAppResourceBinding>,
    pub app_id: String,
    pub operation_id: String,
    pub resources: Vec<crate::AppResourceIdentity>,
    pub docker_bind_cleanup: bool,
}

/// Registration identity captured alongside physical builder resources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderRegistryIdentity {
    pub generation: String,
    pub container_id: String,
}

/// Non-secret evidence persisted before purge starts. Possessing a receipt does
/// not grant a new lease or permission to replay an uncertain remote operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserappDevDeletionReceipt {
    pub runtime: BuilderDeletionSnapshot,
    pub registry: Option<BuilderRegistryIdentity>,
    /// The original builder mutex, distinct from the prod/application mutex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<crate::UserAppOperationLeaseReceipt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collaborators: Vec<BuilderCollaboratorDeletionReceipt>,
    /// None denotes a historical capture without directory witnesses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directories: Option<Vec<crate::storage_contents::CapturedStorageDirectory>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderCollaboratorDeletionReceipt {
    pub runtime: BuilderDeletionSnapshot,
    pub lease: Option<crate::UserAppOperationLeaseReceipt>,
    pub registry: Option<BuilderRegistryIdentity>,
}

/// Read the original destructive capture under its exact execution identity.
/// This validates an admitted write, never the completeness of unrelated history.
pub fn captured_builder_deletion_for_execution(
    operation: &crate::UserAppOperationRecord,
    context: &crate::UserAppExecutionContext,
) -> Result<UserappDevDeletionReceipt, String> {
    context.validate_identity(&context.app_id)?;
    if operation.app_id != context.app_id
        || operation.lifecycle_id != context.lifecycle_id
        || operation.operation_id != context.operation_id
        || operation.executor_id.as_deref() != Some(context.executor_id.as_str())
        || operation.request_fingerprint != context.request_fingerprint
        || operation.state != crate::UserAppOperationState::Running
        || operation.scope != operation.kind.scope()
    {
        return Err("Builder deletion execution identity changed".into());
    }
    use crate::UserAppOperationKind as Kind;
    match operation.kind {
        Kind::PurgeResources | Kind::DeleteApplication => {
            let checkpoint: crate::UserAppDeletionCheckpoint =
                serde_json::from_value(operation.checkpoint.clone())
                    .map_err(|error| error.to_string())?;
            checkpoint.validate_operation(operation)?;
            if checkpoint.context != *context {
                return Err("Original deletion capture executor changed".into());
            }
            checkpoint
                .development
                .ok_or_else(|| "Original deletion has no builder capture".into())
        }
        Kind::DestroyDevStorage | Kind::DestroyProdStorage => {
            let checkpoint: crate::UserAppStorageDestruction =
                serde_json::from_value(operation.checkpoint.clone())
                    .map_err(|error| error.to_string())?;
            checkpoint.validate()?;
            if checkpoint.context != *context
                || checkpoint.production.is_some() != (operation.kind == Kind::DestroyProdStorage)
            {
                return Err("Original storage destruction capture scope changed".into());
            }
            Ok(checkpoint.development)
        }
        Kind::EnsureBuilder
        | Kind::AdoptBuilder
        | Kind::AdoptApplication
        | Kind::StopBuilder
        | Kind::RestartBuilder
        | Kind::Create
        | Kind::StartDeployment
        | Kind::RestartDeployment
        | Kind::Update
        | Kind::Start
        | Kind::Restart
        | Kind::Stop
        | Kind::SetRecyclePolicy
        | Kind::HotDeploy
        | Kind::DeleteCompute
        | Kind::ClearDevStorage
        | Kind::ClearProdStorage
        | Kind::ResetDevDatabasePassword
        | Kind::ResetProdDatabasePassword
        | Kind::PrepareProdDatabase => {
            Err("Current operation does not authorize builder deletion".into())
        }
    }
}

/// Recovery observations never authorize replay of a destructive operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeletionInspection {
    StillHeld(String),
    ConfirmedQuiescent {
        remaining_resources: Vec<crate::AppResourceIdentity>,
    },
    ForeignIdentity(String),
    Unknown(String),
}

/// A captured deletion owns its lifecycle lease until committed or dropped.
#[async_trait]
pub trait UserappDevDeletion: Send {
    fn receipt(&self) -> UserappDevDeletionReceipt;
    /// Inspect the physical target under this ticket's lease. This must not use
    /// registration/address caches or create a missing builder.
    async fn workspace_endpoint(
        &mut self,
        _context: &crate::UserAppExecutionContext,
    ) -> Result<crate::UserAppBuilderWorkspaceEndpoint, String> {
        Err("Physical builder workspace inspection is unsupported".into())
    }
    /// Retain ownership if an external writer's outcome becomes uncertain.
    /// Unsupported adapters must reject before the caller submits that write.
    fn begin_external_mutation(&mut self) -> Result<(), String> {
        Err("External builder mutation fencing is unsupported".into())
    }
    /// Release only after the external writer explicitly confirmed completion.
    async fn finish_external_mutation(&mut self) -> Result<(), String> {
        Err("External builder mutation completion is unsupported".into())
    }
    async fn cleanup(self: Box<Self>) -> Result<(), String>;
}

#[async_trait]
pub trait UserappDevCleanup: Send + Sync {
    /// Capture under the caller's admitted operation. Older adapters keep their
    /// existing capture implementation; concrete destructive adapters validate
    /// the supplied execution before changing resources or retiring registry rows.
    async fn capture_for_execution(
        &self,
        context: &crate::UserAppExecutionContext,
    ) -> Result<Box<dyn UserappDevDeletion>, String> {
        self.capture(&context.app_id).await
    }

    async fn capture_with_lease_for_execution(
        &self,
        context: &crate::UserAppExecutionContext,
        lease: Box<dyn crate::AppOperationLease>,
    ) -> Result<Box<dyn UserappDevDeletion>, String> {
        self.capture_with_lease(&context.app_id, lease).await
    }

    /// Called only after durable execution revocation. Inspect original targets;
    /// never capture a new same-named builder or replay storage deletion.
    async fn inspect_captured(
        &self,
        _context: &crate::UserAppExecutionContext,
        _receipt: &UserappDevDeletionReceipt,
    ) -> Result<DeletionInspection, String> {
        Ok(DeletionInspection::Unknown(
            "Captured development deletion inspection is unsupported".into(),
        ))
    }
    /// Consume a borrowed handle to the caller's existing builder lease. Receipt
    /// data alone is not authorization; unsupported adapters must not reacquire.
    async fn capture_with_lease(
        &self,
        _app_id: &str,
        _lease: Box<dyn crate::AppOperationLease>,
    ) -> Result<Box<dyn UserappDevDeletion>, String> {
        Err("capturing under an existing builder lease is unsupported".into())
    }

    async fn capture(&self, _app_id: &str) -> Result<Box<dyn UserappDevDeletion>, String> {
        Err("identity-bound builder deletion is unsupported".into())
    }
    async fn cleanup(&self, app_id: &str) -> Result<(), String> {
        self.capture(app_id).await?.cleanup().await
    }
}
