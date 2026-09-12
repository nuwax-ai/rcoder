//! Identity-bound UserApp builder deletion contracts.
use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderDeletionSnapshot {
    pub app_id: String,
    pub operation_id: String,
    pub resources: Vec<crate::AppResourceIdentity>,
    pub docker_bind_cleanup: bool,
}

/// A captured deletion owns its lifecycle lease until committed or dropped.
#[async_trait]
pub trait UserappDevDeletion: Send {
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
