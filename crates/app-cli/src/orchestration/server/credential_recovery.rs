//! New Source input is independent from an older artifact's continued existence.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialRecoveryPurpose {
    StartCurrentSource,
    RestoreConfirmedArtifact,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialRecovery {
    NotRequired,
    SupplyCurrentInput { hold: u8 },
}

#[derive(Debug)]
pub(super) enum SourceHistoryProblem {
    MissingArtifact,
    AmbiguousArtifact,
    DeploymentGenerationChanged,
    InconsistentDeploymentIdentity,
}

impl std::fmt::Display for SourceHistoryProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message(shared_types::DEFAULT_LOCALE))
    }
}

impl SourceHistoryProblem {
    fn message(&self, locale: &str) -> String {
        shared_types::t(
            match self {
                Self::MissingArtifact => "error.source_history.missing_artifact",
                Self::AmbiguousArtifact => "error.source_history.ambiguous_artifact",
                Self::DeploymentGenerationChanged => "error.source_history.generation_changed",
                Self::InconsistentDeploymentIdentity => {
                    "error.source_history.inconsistent_identity"
                }
            },
            locale,
        )
    }
}

impl std::error::Error for SourceHistoryProblem {}

/// Diagnostics remain English in receipts/logs; only the HTTP boundary chooses
/// a request language. No process-global locale is changed.
#[derive(Debug)]
enum RecoveryPrerequisite {
    UnconfirmedOutcome,
    CleanupUnconfirmed,
    MissingCredentials,
    SourceWorkspaceUnbound,
    ArtifactHistoryUnavailable,
    ArtifactIdentityMismatch,
    ArtifactTargetMissing,
    ArtifactReleaseChanged,
}

impl RecoveryPrerequisite {
    fn message(&self, locale: &str) -> String {
        shared_types::t(
            match self {
                Self::UnconfirmedOutcome => "error.source_recovery.unconfirmed_outcome",
                Self::CleanupUnconfirmed => "error.source_recovery.cleanup_unconfirmed",
                Self::MissingCredentials => "error.source_recovery.missing_credentials",
                Self::SourceWorkspaceUnbound => "error.source_recovery.workspace_unbound",
                Self::ArtifactHistoryUnavailable => {
                    "error.source_recovery.artifact_history_unavailable"
                }
                Self::ArtifactIdentityMismatch => {
                    "error.source_recovery.artifact_identity_mismatch"
                }
                Self::ArtifactTargetMissing => "error.source_recovery.artifact_target_missing",
                Self::ArtifactReleaseChanged => "error.source_recovery.artifact_release_changed",
            },
            locale,
        )
    }
}
impl std::fmt::Display for RecoveryPrerequisite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message(shared_types::DEFAULT_LOCALE))
    }
}
impl std::error::Error for RecoveryPrerequisite {}

pub(crate) fn localized_recovery_error(error: &anyhow::Error, locale: &str) -> String {
    if let Some(cause) = error.downcast_ref::<RecoveryPrerequisite>() {
        return cause.message(locale);
    }
    if let Some(cause) = error.downcast_ref::<SourceHistoryProblem>() {
        return cause.message(locale);
    }
    if let Some(cause) = error.downcast_ref::<shared_types::SourceRunCredentialError>() {
        return cause.message(locale);
    }
    // Keep concrete I/O and third-party errors intact; never translate by
    // matching English strings or replace them with a generic success/failure.
    format!("{error:#}")
}

pub(super) fn redacted_run_credentials(receipt: &Receipt) -> bool {
    receipt
        .active
        .as_ref()
        .and_then(|active| active.request.as_ref())
        .and_then(|request| request.run_pg.as_ref())
        .is_some_and(|pg| pg.password.is_empty())
}

/// Historical empty passwords are evidence of the former serializer, not a
/// recoverable credential. Never inject that empty value or invent a password.
pub(super) fn restored_run_pg(
    stored: Option<&shared_types::StartPgCredential>,
) -> Result<Option<shared_types::StartPgCredential>> {
    let explicit = match stored {
        Some(pg) if pg.password.is_empty() => {
            tracing::warn!(
                username = %pg.username,
                "historical PostgreSQL input was redacted; restoring only current environment credentials or application configuration"
            );
            None
        }
        pg => pg,
    };
    shared_types::resolve_source_run_pg(explicit).map_err(Into::into)
}

impl CredentialRecovery {
    pub(crate) const fn supplies_credentials(self) -> bool {
        match self {
            Self::NotRequired => false,
            Self::SupplyCurrentInput { .. } => true,
        }
    }
}

impl ServerState {
    /// Read-only evaluation. Admission and dispatch use the same purpose;
    /// only the accepted operation may consume its classified safe hold mask.
    pub(crate) fn credential_recovery(
        &self,
        pg: Option<&shared_types::StartPgCredential>,
        purpose: CredentialRecoveryPurpose,
    ) -> Result<CredentialRecovery> {
        // New explicit input is always validated, even without any recovery
        // hold. An empty new password must not be mistaken for legacy redaction.
        if let Some(pg) = pg {
            shared_types::resolve_source_run_pg(Some(pg))?;
        }
        let hold = self
            .runtime_recovery_hold
            .load(std::sync::atomic::Ordering::Acquire);
        if hold == 0 {
            return Ok(CredentialRecovery::NotRequired);
        }
        anyhow::ensure!(
            hold & !(CREDENTIALS_HOLD | SOURCE_HISTORY_HOLD) == 0,
            RecoveryPrerequisite::UnconfirmedOutcome
        );
        anyhow::ensure!(
            !self
                .shutdown_unconfirmed
                .load(std::sync::atomic::Ordering::Acquire),
            RecoveryPrerequisite::CleanupUnconfirmed
        );
        if hold & CREDENTIALS_HOLD != 0 {
            let pg = pg.ok_or(RecoveryPrerequisite::MissingCredentials)?;
            shared_types::resolve_source_run_pg(Some(pg))?;
        }
        match purpose {
            CredentialRecoveryPurpose::StartCurrentSource => {
                let project = self
                    .execution_project
                    .get()
                    .ok_or(RecoveryPrerequisite::SourceWorkspaceUnbound)?;
                // Validate the source binding independently of diagnostic
                // application migration history.
                resolved_execution_workspace(project, Some(ExecutionTarget::Source), self)?;
                Ok(CredentialRecovery::SupplyCurrentInput { hold })
            }
            CredentialRecoveryPurpose::RestoreConfirmedArtifact => {
                anyhow::ensure!(
                    hold == CREDENTIALS_HOLD,
                    RecoveryPrerequisite::ArtifactHistoryUnavailable
                );
                self.confirm_artifact_credentials_recovery()?;
                Ok(CredentialRecovery::SupplyCurrentInput { hold })
            }
        }
    }

    fn confirm_artifact_credentials_recovery(&self) -> Result<()> {
        let receipt = self
            .journal
            .lock()
            .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?
            .as_ref()
            .and_then(|journal| journal.receipt.clone())
            .context("confirmed deployment journal missing")?;
        let boundary = matches!(
            receipt.boundary,
            Boundary::Active | Boundary::RestoredActive | Boundary::StartupFailed
        ) || (receipt.boundary == Boundary::Preparing
            && receipt.operation.phase == AppCliDeployPhase::Failed);
        anyhow::ensure!(
            receipt.generation == self.generation_value() && boundary,
            RecoveryPrerequisite::ArtifactIdentityMismatch
        );
        let active = receipt
            .active
            .context("confirmed active artifact missing")?;
        let request = active
            .request
            .context("confirmed active artifact request missing")?;
        let workspace = match request.execution_target {
            Some(target) => {
                let project = self
                    .execution_project
                    .get()
                    .context("owner project missing")?;
                resolved_execution_workspace(project, Some(target), self)?
            }
            None => {
                anyhow::ensure!(
                    request.local_path.is_none()
                        && (request.url.starts_with("https://")
                            || request.url.starts_with("http://")),
                    RecoveryPrerequisite::ArtifactTargetMissing
                );
                self.owner_execution_workspace
                    .get()
                    .context("owner execution workspace missing")?
                    .clone()
            }
        };
        let release = crate::manifest::read_release_lock(&workspace)?;
        anyhow::ensure!(
            release.release_id == active.artifact_release_id,
            RecoveryPrerequisite::ArtifactReleaseChanged
        );
        Ok(())
    }
}

#[cfg(test)]
mod localization_tests {
    use super::*;

    #[test]
    fn default_diagnostics_are_english_and_all_supported_catalogs_resolve() {
        for problem in [
            SourceHistoryProblem::MissingArtifact,
            SourceHistoryProblem::AmbiguousArtifact,
            SourceHistoryProblem::DeploymentGenerationChanged,
            SourceHistoryProblem::InconsistentDeploymentIdentity,
        ] {
            let english = problem.to_string();
            assert!(english.is_ascii() && english.starts_with("The previous"));
            assert_eq!(problem.message("fr-FR"), english);
            for locale in shared_types::SUPPORTED_LOCALES {
                let message = problem.message(locale);
                assert!(!message.starts_with("error."));
                assert!(!message.is_empty());
            }
            assert!(problem.message("zh-CN").contains("历史"));
            assert!(problem.message("zh-TW").contains("歷史"));
        }
        for problem in [
            RecoveryPrerequisite::UnconfirmedOutcome,
            RecoveryPrerequisite::CleanupUnconfirmed,
            RecoveryPrerequisite::MissingCredentials,
            RecoveryPrerequisite::SourceWorkspaceUnbound,
            RecoveryPrerequisite::ArtifactHistoryUnavailable,
            RecoveryPrerequisite::ArtifactIdentityMismatch,
            RecoveryPrerequisite::ArtifactTargetMissing,
            RecoveryPrerequisite::ArtifactReleaseChanged,
        ] {
            assert!(problem.to_string().is_ascii());
            for locale in shared_types::SUPPORTED_LOCALES {
                assert!(!problem.message(locale).starts_with("error."));
            }
        }
    }

    #[test]
    fn typed_causes_localize_without_rewriting_unrecognized_contexts() {
        let error =
            anyhow::Error::new(SourceHistoryProblem::MissingArtifact).context("restore owner");
        assert!(localized_recovery_error(&error, "zh-CN").contains("历史制品"));
        assert!(format!("{error:#}").contains("restore owner: The previous"));
        let input = anyhow::Error::new(shared_types::SourceRunCredentialError::InvalidPassword);
        assert!(localized_recovery_error(&input, "zh-TW").contains("密碼"));
        let io = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "source is unreadable",
        ))
        .context("read /workspace/source");
        assert_eq!(localized_recovery_error(&io, "zh-CN"), format!("{io:#}"));
    }
}
