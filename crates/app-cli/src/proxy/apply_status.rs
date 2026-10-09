//! Publication-bound applied graph observation, never candidate-hash authority.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyState {
    Preparing,
    Applied,
    Failed,
    RestartRequired,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApplyFailure {
    pub category: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AppliedPublication {
    pub attempt_id: u64,
    pub operation_id: Option<String>,
    pub config_hash: String,
    pub config_digest: String,
    pub applied_at_unix_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApplyAttempt {
    pub attempt_id: u64,
    pub operation_id: Option<String>,
    pub config_hash: Option<String>,
    pub config_digest: Option<String>,
    pub state: ApplyState,
    pub failure: Option<ApplyFailure>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ApplyStatus {
    pub schema_version: u32,
    pub process_id: u32,
    pub process_instance_id: String,
    pub applied: Option<AppliedPublication>,
    pub last_attempt: Option<ApplyAttempt>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmedPublication {
    pub process_id: u32,
    pub instance_id: String,
    pub publication_id: String,
    pub config_hash: String,
    pub config_digest: String,
    pub attempt_id: u64,
}

impl ApplyStatus {
    pub fn validate_identity(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema_version == 1
                && self.process_id > 0
                && uuid::Uuid::parse_str(&self.process_instance_id).is_ok(),
            "invalid proxy application observation identity"
        );
        Ok(())
    }

    pub fn current_publication(&self) -> anyhow::Result<ConfirmedPublication> {
        self.validate_identity()?;
        let applied = self
            .applied
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("proxy has no Applied graph"))?;
        anyhow::ensure!(
            applied.attempt_id > 0 && applied.applied_at_unix_ms > 0,
            "invalid applied proxy receipt"
        );
        Ok(ConfirmedPublication {
            process_id: self.process_id,
            instance_id: self.process_instance_id.clone(),
            publication_id: applied
                .operation_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("proxy publication identity missing"))?,
            config_hash: applied.config_hash.clone(),
            config_digest: applied.config_digest.clone(),
            attempt_id: applied.attempt_id,
        })
    }

    pub fn confirmed(
        &self,
        expected: &super::compiler::CompileOutcome,
    ) -> anyhow::Result<Option<ConfirmedPublication>> {
        self.validate_identity()?;
        if let Some(attempt) = &self.last_attempt
            && attempt.operation_id.as_deref() == Some(&expected.publication_id)
            && attempt.config_digest.as_deref() == Some(&expected.config_digest)
        {
            match attempt.state {
                ApplyState::Failed | ApplyState::RestartRequired => {
                    let error = attempt
                        .failure
                        .as_ref()
                        .map(|failure| format!("{}: {}", failure.category, failure.message))
                        .unwrap_or_else(|| "proxy publication rejected without details".into());
                    anyhow::bail!(
                        "proxy publication {} failed: {error}",
                        expected.publication_id
                    );
                }
                ApplyState::Preparing | ApplyState::Applied => {}
            }
        }
        let Some(applied) = &self.applied else {
            return Ok(None);
        };
        if applied.operation_id.as_deref() != Some(&expected.publication_id)
            || applied.config_digest != expected.config_digest
            || !super::admin_probe::hashes_match(&expected.expected_hash, &applied.config_hash)
        {
            return Ok(None);
        }
        anyhow::ensure!(
            applied.attempt_id > 0 && applied.applied_at_unix_ms > 0,
            "invalid applied proxy receipt"
        );
        Ok(Some(ConfirmedPublication {
            process_id: self.process_id,
            instance_id: self.process_instance_id.clone(),
            publication_id: expected.publication_id.clone(),
            config_hash: applied.config_hash.clone(),
            config_digest: applied.config_digest.clone(),
            attempt_id: applied.attempt_id,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (ApplyStatus, super::super::compiler::CompileOutcome) {
        let id = uuid::Uuid::new_v4().to_string();
        let outcome = super::super::compiler::CompileOutcome {
            config_path: "candidate.toml".into(),
            expected_hash: "0123ABCD".into(),
            publication_id: id.clone(),
            config_digest: "a".repeat(64),
            entry_probes: vec![],
            business_probes: vec![],
        };
        let status = ApplyStatus {
            schema_version: 1,
            process_id: 123,
            process_instance_id: uuid::Uuid::new_v4().to_string(),
            applied: Some(AppliedPublication {
                attempt_id: 1,
                operation_id: Some(id),
                config_hash: outcome.expected_hash.clone(),
                config_digest: outcome.config_digest.clone(),
                applied_at_unix_ms: 1,
            }),
            last_attempt: None,
        };
        (status, outcome)
    }
    #[test]
    fn hash_only_or_late_previous_applied_never_confirms_new_publication() {
        let (mut status, expected) = fixture();
        assert!(status.confirmed(&expected).unwrap().is_some());
        status.applied.as_mut().unwrap().operation_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(status.confirmed(&expected).unwrap().is_none());
        status.applied.as_mut().unwrap().operation_id = Some(expected.publication_id.clone());
        status.applied.as_mut().unwrap().config_digest = "b".repeat(64);
        assert!(status.confirmed(&expected).unwrap().is_none());
    }
    #[test]
    fn failed_current_attempt_cannot_reuse_an_old_success() {
        let (mut status, expected) = fixture();
        status.last_attempt = Some(ApplyAttempt {
            attempt_id: 2,
            operation_id: Some(expected.publication_id.clone()),
            config_hash: Some(expected.expected_hash.clone()),
            config_digest: Some(expected.config_digest.clone()),
            state: ApplyState::Failed,
            failure: Some(ApplyFailure {
                category: "plugin".into(),
                message: "not compiled".into(),
            }),
        });
        assert!(
            status
                .confirmed(&expected)
                .unwrap_err()
                .to_string()
                .contains("not compiled")
        );
    }
    #[test]
    fn missing_or_invalid_identity_is_a_protocol_error() {
        let (mut status, expected) = fixture();
        status.process_instance_id.clear();
        assert!(status.confirmed(&expected).is_err());
        status.process_instance_id = uuid::Uuid::new_v4().to_string();
        status.schema_version = 2;
        assert!(status.confirmed(&expected).is_err());
        assert!(
            serde_json::from_str::<ApplyStatus>(r#"{"schema_version":1,"config_hash":"0123ABCD"}"#)
                .is_err()
        );
    }
}
