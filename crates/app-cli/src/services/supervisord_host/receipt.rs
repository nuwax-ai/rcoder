//! Structured engine ownership receipts and read-only domain validation.
use super::*;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EngineReceipt {
    pub(super) generation: String,
    pub(super) supervisor_id: String,
    pub(super) socket: PathBuf,
}

pub(super) const ENGINE_RECEIPT: &str = "supervisord-engine.json";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResidentEngineReceipt {
    pub(super) version: u32,
    pub(super) identity: process_utils::command_authority::ResidentIdentity,
    pub(super) runtime_root: PathBuf,
    pub(super) socket: PathBuf,
    pub(super) config: PathBuf,
    pub(super) process: Option<crate::xmlrpc::RunningProcess>,
}

pub(super) const RESIDENT_ENGINE_RECEIPT: &str = "resident-supervisord.json";

#[derive(Debug)]
pub(super) struct EntryOwnershipUnconfirmed(pub(super) String);
impl std::fmt::Display for EntryOwnershipUnconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for EntryOwnershipUnconfirmed {}

/// Record the structured outcome and return it as the error cause; the
/// parent cleanup reads the sidecar for its own classification.
pub(super) fn record_outcome(
    root: &Path,
    outcome: runtime_supervisor::CleanupOutcome,
) -> anyhow::Error {
    let detail = format!("{outcome:?}");
    if let Err(error) = outcome.record(root) {
        return anyhow::anyhow!("persist cleanup outcome failed: {error:#}");
    }
    anyhow::anyhow!("{detail}")
}

pub(super) fn read_resident_engine_receipt(root: &Path) -> Result<Option<ResidentEngineReceipt>> {
    let bytes = match std::fs::read(root.join(RESIDENT_ENGINE_RECEIPT)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read resident engine owner receipt"),
    };
    serde_json::from_slice(&bytes)
        .context("decode resident engine owner receipt")
        .map(Some)
}

pub(super) fn verify_resident_engine_receipt(
    root: &Path,
    identity: &process_utils::command_authority::ResidentIdentity,
    socket: &Path,
) -> Result<Option<ResidentEngineReceipt>> {
    let Some(receipt) = read_resident_engine_receipt(root)? else {
        return Ok(None);
    };
    anyhow::ensure!(
        receipt.version == 1
            && receipt.runtime_root == std::fs::canonicalize(root)?
            && receipt.socket == socket
            && receipt.identity.application_id == identity.application_id
            && receipt.identity.binding == identity.binding
            && receipt.identity.physical_domain == identity.physical_domain
            && receipt.identity.process_epoch == identity.process_epoch,
        "resident engine belongs to another application, workspace, physical domain or incarnation"
    );
    Ok(Some(receipt))
}
