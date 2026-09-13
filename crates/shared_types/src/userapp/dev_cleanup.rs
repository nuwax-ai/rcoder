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
    async fn capture(&self, _app_id: &str) -> Result<Box<dyn UserappDevDeletion>, String> {
        Err("identity-bound builder deletion is unsupported".into())
    }
    async fn cleanup(&self, app_id: &str) -> Result<(), String> {
        self.capture(app_id).await?.cleanup().await
    }
}
