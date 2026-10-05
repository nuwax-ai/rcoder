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
        f.write_str(match self {
            Self::MissingArtifact => "历史制品目录已不存在；等待新的源码启动或明确部署请求",
            Self::AmbiguousArtifact => {
                "历史制品目录无法唯一定位；新的源码请求仍可使用当前授权源码根"
            }
            Self::DeploymentGenerationChanged => {
                "历史部署代次与当前管理实例不匹配；等待新的源码启动或明确部署请求"
            }
            Self::InconsistentDeploymentIdentity => {
                "历史部署身份字段不一致；保留原记录，等待新的源码启动或明确部署请求"
            }
        })
    }
}

impl std::error::Error for SourceHistoryProblem {}

pub(super) fn redacted_run_credentials(receipt: &Receipt) -> bool {
    receipt
        .active
        .as_ref()
        .and_then(|active| active.request.as_ref())
        .and_then(|request| request.run_pg.as_ref())
        .is_some_and(|pg| pg.password.is_empty())
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
        let hold = self
            .runtime_recovery_hold
            .load(std::sync::atomic::Ordering::Acquire);
        if hold == 0 {
            return Ok(CredentialRecovery::NotRequired);
        }
        anyhow::ensure!(
            hold & !(CREDENTIALS_HOLD | SOURCE_HISTORY_HOLD) == 0,
            "运行恢复仍有未确认的写入结果；请查询具体恢复状态后重试"
        );
        anyhow::ensure!(
            !self
                .shutdown_unconfirmed
                .load(std::sync::atomic::Ordering::Acquire),
            "旧服务清理尚未确认；当前请求不能提前启动新服务"
        );
        if hold & CREDENTIALS_HOLD != 0 {
            let pg = pg.context("平台未提供数据库运行凭据，业务暂未启动；请重试")?;
            shared_types::resolve_source_run_pg(Some(pg))?;
        }
        match purpose {
            CredentialRecoveryPurpose::StartCurrentSource => {
                let project = self
                    .execution_project
                    .get()
                    .context("当前owner尚未绑定源码工作区")?;
                let workspace =
                    resolved_execution_workspace(project, Some(ExecutionTarget::Source), self)?;
                // begin() rechecks pending SQL under the migration lease before
                // execution. This eligibility check never marks old SQL complete.
                crate::migration_journal::require_confirmed_migrations(&workspace)?;
                Ok(CredentialRecovery::SupplyCurrentInput { hold })
            }
            CredentialRecoveryPurpose::RestoreConfirmedArtifact => {
                anyhow::ensure!(
                    hold == CREDENTIALS_HOLD,
                    "历史制品目录缺失或身份不明；请使用新的源码启动或明确的新部署请求"
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
            "原制品恢复的部署身份或确认阶段不匹配"
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
                    "原本地制品缺少已确认的执行目录"
                );
                self.owner_execution_workspace
                    .get()
                    .context("owner execution workspace missing")?
                    .clone()
            }
        };
        crate::migration_journal::require_confirmed_migrations(&workspace)?;
        let release = crate::manifest::read_release_lock(&workspace)?;
        anyhow::ensure!(
            release.release_id == active.artifact_release_id,
            "原制品恢复的release身份已变化；请使用明确的新部署请求"
        );
        Ok(())
    }
}
