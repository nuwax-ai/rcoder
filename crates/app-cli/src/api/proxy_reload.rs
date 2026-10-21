//! Reload request identity and terminal evidence are separate from strict owner
//! and engine receipts. A historical unfinished record never grants admission
//! and never blocks a new request after authoritative owner recovery.
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::{Context, Result};
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shared_types::{ProxyPhase, ProxyReloadOperation, ProxyReloadState};

use super::{AppState, envelope, proxy::ProxyReloadData};

static RELOAD_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static LEGACY_OWNER: OnceLock<String> = OnceLock::new();

pub(super) fn gate() -> Result<tokio::sync::MutexGuard<'static, ()>> {
    RELOAD_GATE
        .try_lock()
        .context("another proxy reload is in progress")
}

pub(super) fn request_id(headers: &HeaderMap) -> Result<String> {
    match requested_id(headers)? {
        Some(id) => Ok(id),
        None => Ok(uuid::Uuid::new_v4().to_string()),
    }
}

pub(super) fn requested_id(headers: &HeaderMap) -> Result<Option<String>> {
    let mut values = headers.get_all("x-request-id").iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    anyhow::ensure!(values.next().is_none(), "X-Request-ID must occur once");
    let id = value.to_str().context("X-Request-ID must be ASCII")?;
    anyhow::ensure!(
        !id.is_empty() && id.len() <= 128 && id.bytes().all(|byte| (0x21..=0x7e).contains(&byte)),
        "X-Request-ID must contain 1..128 printable ASCII characters without whitespace"
    );
    Ok(Some(id.to_owned()))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RequestMetadata {
    workspace: PathBuf,
    dev_profile: bool,
    release_id: String,
    release_fingerprint: String,
    application_id: Option<String>,
    workspace_id: Option<String>,
    deployment_generation_id: Option<String>,
}

impl RequestMetadata {
    pub(super) fn capture(state: &AppState) -> Result<Self> {
        let context = state
            .server
            .proxy_context()
            .context("no active proxy profile")?;
        let release = state.server.release().context("no release deployed")?;
        let identity = state
            .server
            .runtime_kernel()
            .map(|kernel| kernel.identity().clone());
        Ok(Self {
            workspace: context.workspace,
            dev_profile: context.dev_profile,
            release_id: release.release_id.clone(),
            release_fingerprint: hex::encode(Sha256::digest(serde_json::to_vec(&release)?)),
            application_id: identity
                .as_ref()
                .map(|identity| identity.application_id.clone()),
            workspace_id: identity
                .as_ref()
                .map(|identity| identity.workspace_id.clone()),
            deployment_generation_id: identity.map(|identity| identity.deployment_generation_id),
        })
    }
}

pub(super) fn owner_id(state: &AppState) -> String {
    state
        .server
        .runtime_kernel()
        .map(|kernel| kernel.identity().runtime_instance_id.clone())
        .unwrap_or_else(|| {
            LEGACY_OWNER
                .get_or_init(|| uuid::Uuid::new_v4().to_string())
                .clone()
        })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReloadRecord {
    schema_version: u32,
    request_id: String,
    metadata: RequestMetadata,
    owner_id: String,
    pub(super) operation: ProxyReloadOperation,
    /// Captured after complete preflight, rather than recompiled on replay.
    candidate: Option<ProxyReloadData>,
    pub(super) result: Option<ProxyReloadData>,
    http_status: u16,
}

/// This pointer is advanced only by admission, never inferred from timestamps,
/// directory order or an unrelated current proxy publication.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReloadHead {
    schema_version: u32,
    request_id: String,
    metadata: RequestMetadata,
}

pub(super) fn root(state: &AppState) -> Result<PathBuf> {
    Ok(crate::proxy::compiler::runtime_root(&state.log_dir)?.join("reload-operations"))
}

fn record_path(root: &Path, id: &str) -> PathBuf {
    root.join(format!(
        "{}.json",
        hex::encode(Sha256::digest(id.as_bytes()))
    ))
}

pub(super) fn read(root: &Path, id: &str) -> Result<Option<ReloadRecord>> {
    let path = record_path(root, id);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read proxy reload evidence"),
    };
    let record: ReloadRecord =
        serde_json::from_slice(&bytes).context("decode proxy reload evidence")?;
    anyhow::ensure!(
        record.schema_version == 1
            && record.request_id == id
            && record.operation.operation_id == id,
        "proxy reload evidence identity mismatch"
    );
    if let Some(candidate) = &record.candidate {
        anyhow::ensure!(
            candidate.operation_id == id,
            "proxy reload candidate belongs to another request"
        );
    }
    if record.operation.state == ProxyReloadState::Succeeded {
        let result = record
            .result
            .as_ref()
            .context("succeeded proxy reload has no original result")?;
        validate_success(id, result)?;
    }
    Ok(Some(record))
}

pub(super) fn current_id(state: &AppState) -> Result<Option<String>> {
    let directory = root(state)?;
    let bytes = match std::fs::read(directory.join("head.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read explicit proxy reload head"),
    };
    let head: ReloadHead =
        serde_json::from_slice(&bytes).context("decode explicit proxy reload head")?;
    anyhow::ensure!(
        head.schema_version == 1,
        "unsupported proxy reload head version"
    );
    let record = read(&directory, &head.request_id)?
        .context("proxy reload head points to missing evidence")?;
    anyhow::ensure!(
        head.metadata == record.metadata,
        "proxy reload head metadata mismatch"
    );
    if let Some(kernel) = state.server.runtime_kernel() {
        let identity = kernel.identity();
        anyhow::ensure!(
            head.metadata.application_id.as_deref() == Some(identity.application_id.as_str())
                && head.metadata.workspace_id.as_deref() == Some(identity.workspace_id.as_str()),
            "proxy reload head belongs to another application workspace"
        );
    } else {
        let context = state
            .server
            .proxy_context()
            .context("proxy reload head has no current workspace binding")?;
        anyhow::ensure!(
            runtime_state_layout::resolve_project_origin(&head.metadata.workspace)?
                == runtime_state_layout::resolve_project_origin(&context.workspace)?,
            "proxy reload head belongs to another project origin"
        );
    }
    Ok(Some(head.request_id))
}

fn write_head(root: &Path, record: &ReloadRecord) -> Result<()> {
    let head = ReloadHead {
        schema_version: 1,
        request_id: record.request_id.clone(),
        metadata: record.metadata.clone(),
    };
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer(&mut file, &head)?;
    file.flush()?;
    file.as_file().sync_all()?;
    process_utils::atomic_file::persist(file, &root.join("head.json"))
        .context("publish explicit proxy reload head")?;
    #[cfg(unix)]
    std::fs::File::open(root)?.sync_all()?;
    Ok(())
}

fn validate_success(id: &str, data: &ProxyReloadData) -> Result<()> {
    let confirmation = data
        .confirmation
        .as_ref()
        .context("successful reload has no official confirmation")?;
    anyhow::ensure!(
        data.operation_id == id
            && data.state == ProxyReloadState::Succeeded
            && data.reloaded
            && !data.verified
            && data.process_instance_id.is_empty()
            && confirmation.confirmed
            && confirmation.operation_id == id
            && matches!(
                confirmation.phase,
                ProxyPhase::Serving | ProxyPhase::Standby
            )
            && confirmation.config_digest.as_deref() == Some(data.config_digest.as_str())
            && confirmation
                .proxy_instance_id
                .as_deref()
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()),
        "proxy reload success evidence is inconsistent"
    );
    Ok(())
}

fn write(root: &Path, record: &ReloadRecord, create: bool) -> Result<()> {
    std::fs::create_dir_all(root).context("create independent proxy reload journal")?;
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    serde_json::to_writer(&mut file, record)?;
    file.flush()?;
    file.as_file().sync_all()?;
    let path = record_path(root, &record.request_id);
    if create {
        file.persist_noclobber(&path)
            .context("admit immutable proxy reload request")?;
    } else {
        process_utils::atomic_file::persist(file, &path).context("persist proxy reload result")?;
    }
    #[cfg(unix)]
    std::fs::File::open(root)?.sync_all()?;
    Ok(())
}

impl ReloadRecord {
    pub(super) fn check_metadata(&self, metadata: &RequestMetadata) -> Result<()> {
        anyhow::ensure!(
            &self.metadata == metadata,
            "X-Request-ID belongs to different proxy request metadata"
        );
        Ok(())
    }

    pub(super) fn observed_operation(&self, state: &AppState) -> ProxyReloadOperation {
        let mut operation = self.operation.clone();
        if matches!(
            operation.state,
            ProxyReloadState::Pending | ProxyReloadState::Running
        ) {
            if self.owner_id != owner_id(state) {
                operation.state = ProxyReloadState::RecoveryRequired;
                operation.phase = ProxyPhase::RecoveryRequired;
                operation.error_code = Some("ERR_RECOVERY_REQUIRED".into());
                operation.error_message = Some("The original reload owner exited before its outcome was recorded; inspect owner recovery before retrying with a new request identity".into());
            } else if let Some(confirmation) = crate::proxy::compiler::official_confirmation()
                && confirmation.operation_id == operation.operation_id
            {
                operation.phase = confirmation.phase;
            }
        }
        operation
    }

    pub(super) fn response(&self, state: &AppState, replayed: bool) -> Response {
        let operation = self.observed_operation(state);
        if operation.state == ProxyReloadState::Succeeded {
            if let Some(mut result) = self.result.clone() {
                result.replayed = replayed;
                return envelope::ok(StatusCode::OK, result);
            }
            return failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                "ERR_RECOVERY_REQUIRED",
                "Succeeded reload evidence is missing its original result",
                operation,
            );
        }
        if matches!(
            operation.state,
            ProxyReloadState::Pending | ProxyReloadState::Running
        ) {
            let mut result = self
                .candidate
                .clone()
                .unwrap_or_else(|| ProxyReloadData::pending(&operation));
            result.state = operation.state;
            result.replayed = replayed;
            return envelope::ok(StatusCode::ACCEPTED, result);
        }
        let status = if operation.state == ProxyReloadState::RecoveryRequired {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::from_u16(self.http_status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
        };
        failure(
            status,
            operation
                .error_code
                .as_deref()
                .unwrap_or("ERR_RECOVERY_REQUIRED"),
            operation
                .error_message
                .as_deref()
                .unwrap_or("Proxy reload outcome remains unknown"),
            operation.clone(),
        )
    }
}

pub(super) struct Claim {
    root: PathBuf,
    pub(super) record: ReloadRecord,
    completed: bool,
}

impl Claim {
    pub(super) fn admit(state: &AppState, id: String, metadata: RequestMetadata) -> Result<Self> {
        let root = root(state)?;
        let record = ReloadRecord {
            schema_version: 1,
            request_id: id.clone(),
            metadata,
            owner_id: owner_id(state),
            operation: ProxyReloadOperation {
                operation_id: id,
                state: ProxyReloadState::Pending,
                phase: ProxyPhase::Preparing,
                error_code: None,
                error_message: None,
            },
            candidate: None,
            result: None,
            http_status: StatusCode::ACCEPTED.as_u16(),
        };
        write(&root, &record, true)?;
        let mut claim = Self {
            root,
            record,
            completed: false,
        };
        if let Err(error) = write_head(&claim.root, &claim.record) {
            // No proxy mutation was dispatched. Persist that known failure;
            // never leave an admitted-looking Pending request by losing head.
            if let Err(persistence) = claim.failed(&error, false) {
                return Err(persistence)
                    .context(format!("reload head publication failed: {error:#}"));
            }
            return Err(error);
        }
        Ok(claim)
    }

    pub(super) fn running(&mut self, data: ProxyReloadData) -> Result<()> {
        self.record.operation.state = ProxyReloadState::Running;
        self.record.operation.phase = ProxyPhase::Preparing;
        self.record.candidate = Some(data);
        write(&self.root, &self.record, false)
    }

    pub(super) fn succeeded(&mut self, data: ProxyReloadData) -> Result<()> {
        validate_success(&self.record.request_id, &data)?;
        self.record.operation.state = ProxyReloadState::Succeeded;
        self.record.operation.phase = data
            .confirmation
            .as_ref()
            .map(|value| value.phase)
            .unwrap_or(ProxyPhase::Serving);
        self.record.result = Some(data);
        self.record.http_status = 200;
        write(&self.root, &self.record, false)?;
        self.completed = true;
        Ok(())
    }

    pub(super) fn failed(&mut self, error: &anyhow::Error, unconfirmed: bool) -> Result<()> {
        self.record.operation.state = if unconfirmed {
            ProxyReloadState::RecoveryRequired
        } else {
            ProxyReloadState::Failed
        };
        self.record.operation.phase = if unconfirmed {
            ProxyPhase::RecoveryRequired
        } else {
            ProxyPhase::Failed
        };
        let busy = error.is::<crate::proxy::compiler::ProxyOperationBusy>();
        self.record.operation.error_code = Some(
            if unconfirmed {
                "ERR_RECOVERY_REQUIRED"
            } else if busy {
                "ERR_OPERATION_IN_PROGRESS"
            } else {
                "PINGAP_CONFIG_INVALID"
            }
            .into(),
        );
        self.record.operation.error_message = Some(format!("{error:#}"));
        self.record.http_status = if unconfirmed {
            503
        } else if busy {
            409
        } else {
            400
        };
        write(&self.root, &self.record, false)?;
        self.completed = true;
        Ok(())
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if !self.completed {
            self.record.operation.state = ProxyReloadState::RecoveryRequired;
            self.record.operation.phase = ProxyPhase::RecoveryRequired;
            self.record.operation.error_code = Some("ERR_RECOVERY_REQUIRED".into());
            self.record.operation.error_message = Some(
                "Proxy reload execution ended before its terminal result was persisted".into(),
            );
            self.record.http_status = 503;
            if let Err(error) = write(&self.root, &self.record, false) {
                tracing::error!(operation_id = %self.record.request_id, "persist interrupted proxy reload: {error:#}");
            }
        }
    }
}

pub(super) fn failure(
    status: StatusCode,
    code: &str,
    message: &str,
    operation: ProxyReloadOperation,
) -> Response {
    (
        status,
        Json(envelope::HttpResult {
            code: code.into(),
            message: message.into(),
            data: Some(operation),
            tid: None,
            success: false,
        }),
    )
        .into_response()
}

#[cfg(test)]
#[path = "proxy_reload_tests.rs"]
mod tests;
