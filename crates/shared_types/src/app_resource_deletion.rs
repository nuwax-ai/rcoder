//! Physical resource identities captured before an application deletion begins.
//! These receipts must never be reconstructed from names after a mutation.
use serde::{Deserialize, Serialize};

/// An application-wide runtime lease. Dropping it is cancellation-safe; successful
/// callers explicitly await release before reporting completion.
#[async_trait::async_trait]
pub trait AppOperationLease: Send + Sync {
    async fn release(self: Box<Self>) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppResourceKind {
    Container,
    Deployment,
    StatefulSet,
    Service,
    ConfigMap,
    Secret,
    HttpRoute,
    PersistentVolumeClaim,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppResourceIdentity {
    pub kind: AppResourceKind,
    pub name: String,
    pub uid: String,
    pub resource_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppDeletionSnapshot {
    pub app_id: String,
    pub operation_id: String,
    pub resources: Vec<AppResourceIdentity>,
}

/// Durable mutation ownership inside an exclusively locked application lock file.
/// A nonempty marker is never reclaimed by time or process liveness heuristics.
pub struct AppFileMutationMarker {
    operation_id: String,
}

impl Default for AppFileMutationMarker {
    fn default() -> Self {
        Self::new()
    }
}
impl AppFileMutationMarker {
    pub fn new() -> Self {
        Self {
            operation_id: uuid::Uuid::new_v4().to_string(),
        }
    }
    pub fn check_clean(file: &std::fs::File) -> std::io::Result<()> {
        if file.metadata()?.len() == 0 {
            return Ok(());
        }
        Err(std::io::Error::other(
            "incomplete application mutation requires operator recovery",
        ))
    }
    pub fn begin(&self, mut file: &std::fs::File) -> std::io::Result<()> {
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut owner = String::new();
        file.seek(SeekFrom::Start(0))?;
        file.read_to_string(&mut owner)?;
        if owner == self.operation_id {
            return Ok(());
        }
        if !owner.is_empty() {
            return Err(std::io::Error::other(
                "application mutation ownership changed",
            ));
        }
        file.seek(SeekFrom::Start(0))?;
        file.write_all(self.operation_id.as_bytes())?;
        file.sync_all()
    }
    pub fn complete(&self, mut file: &std::fs::File) -> std::io::Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let mut owner = String::new();
        file.seek(SeekFrom::Start(0))?;
        file.read_to_string(&mut owner)?;
        if !owner.is_empty() && owner != self.operation_id {
            return Err(std::io::Error::other(
                "application mutation ownership changed",
            ));
        }
        file.set_len(0)?;
        file.sync_all()
    }
}

/// Docker production identity label, shared by create, inspect and deletion.
pub const USERAPP_DOCKER_APP_ID_LABEL: &str = "app-id";

/// Preparation completed without changing the running application's container
/// or stored content. Image-cache and empty-directory preparation may have run;
/// there is no outstanding application mutation to retain the operation lease for.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct AppPreparationFailure {
    pub message: String,
}
