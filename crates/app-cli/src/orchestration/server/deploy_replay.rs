//! HTTP deployment idempotency. Input hashes never serialize credential values
//! into the journal; the history and active receipt commit in one atomic file.
use super::{AdmissionError, DeployRequest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shared_types::AppDeploymentOperation;

pub(super) type History = std::collections::BTreeMap<String, Replay>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Replay {
    // Old receipts redact passwords, so their original fingerprint may be unknown.
    pub fingerprint: Option<String>,
    pub operation: AppDeploymentOperation,
}

#[derive(Debug)]
pub(crate) enum DeployAdmission {
    Accepted,
    Replayed(AppDeploymentOperation),
}

pub(super) fn fingerprint(request: &DeployRequest) -> Result<String, AdmissionError> {
    // Hash the stable execution input explicitly; process-local fields and
    // future unrelated receipt metadata must not change replay identity.
    let input = serde_json::to_vec(&(
        &request.url,
        &request.release_id,
        &request.sha256,
        &request.local_path,
        request.execution_target,
        request
            .run_pg
            .as_ref()
            .map(|pg| (&pg.username, &pg.password)),
    ))
    .map_err(|error| format!("fingerprint deployment input: {error}"))?;
    Ok(hex::encode(Sha256::digest(input)))
}

pub(super) fn check(
    replay: &Replay,
    fingerprint: &str,
) -> Result<AppDeploymentOperation, AdmissionError> {
    match replay.fingerprint.as_deref() {
        Some(saved) if saved == fingerprint => Ok(replay.operation.clone()),
        Some(_) => Err(AdmissionError::Conflict("operation_id was already used with different deployment input".into())),
        None => Err(AdmissionError::Conflict("legacy operation input cannot be verified; inspect the original operation or deploy with a new operation_id".into())),
    }
}
