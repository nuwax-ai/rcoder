//! Native SQLite/real-file-lease worker: unreceipted legacy marker safety only.
//! Does not prove recovery of the newer receipted terminal release protocol.
use anyhow::{Context, Result, bail};
use rcoder_storage::userapp_lifecycle::SqliteUserAppStore;
use shared_types::{
    AppFileMutationMarker, AppOperationLease, UserAppAdmission, UserAppAdmissionOutcome,
    UserAppControlCommand, UserAppDeletionCheckpoint, UserAppDeletionStage, UserAppLifecycleStore,
    UserAppOperationKind, UserAppOperationProgress, UserAppOperationRecord, UserAppOperationState,
};
use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
};

struct BlockedFileLease {
    file: std::fs::File,
    marker: AppFileMutationMarker,
    ready: PathBuf,
}
#[async_trait::async_trait]
impl AppOperationLease for BlockedFileLease {
    async fn release(self: Box<Self>) -> Result<(), String> {
        std::fs::write(&self.ready, b"terminal-committed-before-release")
            .map_err(|error| error.to_string())?;
        // Parent observes the actual committed DB row and nonempty locked file,
        // then SIGKILLs this OS process. No sleep guesses the commit window.
        std::future::pending::<()>().await;
        self.marker
            .complete(&self.file)
            .map_err(|error| error.to_string())?;
        self.file.unlock().map_err(|error| error.to_string())
    }
}

fn request() -> UserAppAdmission {
    UserAppAdmission {
        runtime_policy_on_success: None,
        metadata: None,
        command: Some(UserAppControlCommand::DeleteResources {
            purge: false,
            expected_resource_version: None,
        }),
        app_id: "native-crash-app".into(),
        lifecycle_id: None,
        operation_id: "native-delete-operation".into(),
        request_id: Some("native-delete-request".into()),
        request_fingerprint: "a".repeat(64),
        kind: UserAppOperationKind::DeleteCompute,
    }
}
fn progress(
    operation: &UserAppOperationRecord,
    state: UserAppOperationState,
    checkpoint: serde_json::Value,
) -> UserAppOperationProgress {
    UserAppOperationProgress {
        app_id: operation.app_id.clone(),
        operation_id: operation.operation_id.clone(),
        lifecycle_id: operation.lifecycle_id.clone(),
        expected_revision: operation.revision,
        executor_id: "native-worker".into(),
        state,
        step: "native-delete".into(),
        checkpoint,
        error_code: None,
        error_message: None,
    }
}
async fn execute(root: &Path, store: &SqliteUserAppStore) -> Result<()> {
    let intent = request();
    let identity = store.ensure_identity(&intent.app_id).await?;
    let UserAppAdmissionOutcome::Accepted(operation) = store.admit(&intent).await? else {
        bail!("native worker must start with a new admission");
    };
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(root.join("operation.lock"))?;
    file.lock()?;
    let marker = AppFileMutationMarker::for_operation(&operation.operation_id)?;
    AppFileMutationMarker::check_clean(&file)?;
    marker.begin(&file)?;
    let mut running = store
        .advance(&progress(
            &operation,
            UserAppOperationState::Running,
            serde_json::Value::Null,
        ))
        .await?;
    let mut evidence = UserAppDeletionCheckpoint {
        schema_version: 1,
        stage: UserAppDeletionStage::Captured,
        kind: intent.kind,
        context: shared_types::UserAppExecutionContext {
            app_id: intent.app_id.clone(),
            lifecycle_id: identity.lifecycle_id,
            operation_id: operation.operation_id.clone(),
            executor_id: "native-worker".into(),
            request_fingerprint: intent.request_fingerprint,
        },
        production: shared_types::AppDeletionSnapshot {
            app_id: intent.app_id,
            operation_id: operation.operation_id.clone(),
            resources: vec![],
        },
        development: None,
    };
    running = store
        .advance(&progress(
            &running,
            UserAppOperationState::Running,
            serde_json::to_value(&evidence)?,
        ))
        .await?;
    // Test adapter's concrete side effect is a run-owned file removal. It is not
    // represented as a Docker resource and cannot prove Docker reconciliation.
    std::fs::remove_file(root.join("payload"))?;
    std::fs::write(root.join("execution-count"), b"1")?;
    evidence.stage = UserAppDeletionStage::ComputeRemoved;
    running = store
        .advance(&progress(
            &running,
            UserAppOperationState::Running,
            serde_json::to_value(&evidence)?,
        ))
        .await?;
    store
        .advance(&progress(
            &running,
            UserAppOperationState::Succeeded,
            running.checkpoint.clone(),
        ))
        .await?;
    Box::new(BlockedFileLease {
        file,
        marker,
        ready: root.join("release-barrier"),
    })
    .release()
    .await
    .map_err(anyhow::Error::msg)
}
async fn verify(root: &Path, store: &SqliteUserAppStore) -> Result<()> {
    let UserAppAdmissionOutcome::Existing(operation) = store.admit(&request()).await? else {
        bail!("terminal retry was admitted for execution again");
    };
    if operation.state != UserAppOperationState::Succeeded
        || !store.unfinished_operations(None, 128).await?.is_empty()
    {
        bail!("terminal operation reappeared in the recovery queue");
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("operation.lock"))?;
    file.lock()?; // SIGKILL released the OS lock, but not durable mutation ownership.
    if AppFileMutationMarker::check_clean(&file).is_ok() {
        bail!("abandoned durable marker was silently cleared");
    }
    let next = AppFileMutationMarker::for_operation("replacement-operation")?;
    if next.begin(&file).is_ok()
        || std::fs::read_to_string(root.join("operation.lock"))? != operation.operation_id
    {
        bail!("replacement executor reclaimed the unfinished file marker");
    }
    if root.join("payload").exists() || std::fs::read(root.join("execution-count"))? != b"1" {
        bail!("completed side effect was repeated or restored");
    }
    println!(
        "{}",
        serde_json::json!({"terminal_not_reexecuted": true, "marker_retained": true,
        "operation_id": operation.operation_id, "evidence_level": "native_process_sqlite_file_lease"})
    );
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let mode = arguments.next().context("mode is required")?;
    let root = PathBuf::from(
        arguments
            .next()
            .context("owned data directory is required")?,
    );
    if !root.is_absolute() || arguments.next().is_some() {
        bail!("invalid native worker arguments");
    }
    let store = SqliteUserAppStore::open_exclusive(&root.join("userapp.sqlite3")).await?;
    match mode.as_str() {
        "execute" => execute(&root, &store).await,
        "verify" => verify(&root, &store).await,
        _ => bail!("invalid worker mode"),
    }
}
