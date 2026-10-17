//! Failure reporting follows the last durable phase, never a speculative next one.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ComputeFailureDisposition {
    Preserve,
    Record {
        state: ComputeControlState,
        stage: ComputeControlStage,
    },
}

impl ComputeFailureDisposition {
    pub(super) fn decide(record: &ComputeControlRecord, effects_settled: bool) -> Result<Self> {
        // Pending/history/terminal records are not executor progress. In particular,
        // receipt cleanup must not turn an already committed success into failure.
        match record.state {
            ComputeControlState::Running => {}
            ComputeControlState::Pending
            | ComputeControlState::RecoveryRequired
            | ComputeControlState::Succeeded
            | ComputeControlState::Failed
            | ComputeControlState::Superseded => return Ok(Self::Preserve),
        }
        let stage = ComputeControlStage::from_storage_value(&record.stage)
            .context("Cannot report compute failure for an unknown active phase")?;
        let state = match stage {
            ComputeControlStage::DrainingPrevious => {
                if record.lease.is_none() && effects_settled {
                    ComputeControlState::Failed
                } else {
                    ComputeControlState::RecoveryRequired
                }
            }
            ComputeControlStage::Stopping
            | ComputeControlStage::Stopped
            | ComputeControlStage::Starting
            | ComputeControlStage::Verifying
            | ComputeControlStage::Completed => ComputeControlState::RecoveryRequired,
        };
        // This is a pure policy check. Identity, revision, lease and evidence
        // checks remain authoritative in the existing storage transaction.
        validate_compute_progress_transition(
            record.action,
            Some(stage),
            state,
            stage,
            ComputeDiagnosticValue::NonEmpty,
            ComputeDiagnosticValue::NonEmpty,
            record.lease.is_some(),
        )
        .map_err(|error| anyhow!(error.message()))?;
        validate_compute_mutation_evidence(
            stage,
            record.lease.is_some(),
            record.checkpoint.is_object(),
        )
        .map_err(|error| anyhow!(error.message()))?;
        Ok(Self::Record { state, stage })
    }
}

/// Returns false only when no executor update is appropriate, including a lost
/// ownership/revision race. Observation failures must not masquerade as that race.
pub(super) async fn record(
    state: &AppState,
    record: &ComputeControlRecord,
    identity: &ComputeExecutorIdentity,
    effects_settled: bool,
    original: &anyhow::Error,
) -> Result<bool> {
    let (next_state, stage) = match ComputeFailureDisposition::decide(record, effects_settled)? {
        ComputeFailureDisposition::Record { state, stage } => (state, stage),
        ComputeFailureDisposition::Preserve => return Ok(false),
    };
    match state.userapp_store.check_compute_executor(identity).await {
        Ok(()) => {}
        Err(error) if policy::is_version_conflict(&error) => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let failure = ComputeControlProgress {
        identity: identity.clone(),
        expected_revision: record.revision,
        state: next_state,
        stage,
        checkpoint: record.checkpoint.clone(),
        error_code: Some(ERR_BACKEND_ERROR.into()),
        error_message: Some(format!("{original:#}")),
    };
    match state.userapp_store.advance_compute_control(&failure).await {
        Ok(_) => Ok(true),
        Err(error) if policy::is_version_conflict(&error) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn with_reporting_error(
    original: anyhow::Error,
    reporting: Result<()>,
) -> anyhow::Error {
    match reporting {
        Ok(()) => original,
        Err(error) => original.context(format!(
            "Compute failure reporting or original-receipt reconciliation also failed: {error:#}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> ComputeControlRecord {
        ComputeControlRecord {
            app_id: "failure-policy".into(),
            lifecycle_id: "original-lifecycle".into(),
            request_id: "original-request".into(),
            scope: UserAppOperationScope::Dev,
            operation_id: "original-operation".into(),
            request_fingerprint: "a".repeat(64),
            generation: 1,
            revision: 1,
            action: ComputeControlAction::Restart,
            state: ComputeControlState::Running,
            executor_id: Some("original-executor".into()),
            stage: "draining_previous".into(),
            checkpoint: serde_json::json!({"retained_history": "unchanged"}),
            error_code: None,
            error_message: None,
            lease: None,
            interrupted_operations: vec![],
            created_at: chrono::Utc::now(),
        }
    }

    fn lease() -> UserAppOperationLeaseReceipt {
        UserAppOperationLeaseReceipt::Kubernetes {
            service_type: ServiceType::UserappBuilder,
            namespace: "failure-policy".into(),
            name: "original-lease".into(),
            uid: "original-lease-uid".into(),
            resource_version: "1".into(),
            token: "original-executor".into(),
        }
    }

    #[test]
    fn decisions_match_existing_storage_policy_without_changing_the_snapshot() {
        for action in [ComputeControlAction::Stop, ComputeControlAction::Restart] {
            for stage in [
                ComputeControlStage::DrainingPrevious,
                ComputeControlStage::Stopping,
                ComputeControlStage::Stopped,
                ComputeControlStage::Starting,
                ComputeControlStage::Verifying,
            ] {
                for held in [false, true] {
                    for settled in [false, true] {
                        let mut record = record();
                        record.action = action;
                        record.stage = stage.as_str().into();
                        record.lease = held.then(lease);
                        let before = serde_json::to_value(&record).unwrap();
                        let decision = ComputeFailureDisposition::decide(&record, settled);
                        if (action == ComputeControlAction::Stop
                            && matches!(
                                stage,
                                ComputeControlStage::Starting | ComputeControlStage::Verifying
                            ))
                            || (stage != ComputeControlStage::DrainingPrevious && !held)
                        {
                            assert!(decision.is_err());
                        } else {
                            let expected = if stage == ComputeControlStage::DrainingPrevious
                                && !held
                                && settled
                            {
                                ComputeControlState::Failed
                            } else {
                                ComputeControlState::RecoveryRequired
                            };
                            assert_eq!(
                                decision.unwrap(),
                                ComputeFailureDisposition::Record {
                                    state: expected,
                                    stage,
                                }
                            );
                        }
                        assert_eq!(serde_json::to_value(&record).unwrap(), before);
                    }
                }
            }
        }
    }

    #[test]
    fn non_executor_and_historical_states_are_preserved_without_parsing_their_phase() {
        for state in [
            ComputeControlState::Pending,
            ComputeControlState::RecoveryRequired,
            ComputeControlState::Succeeded,
            ComputeControlState::Failed,
            ComputeControlState::Superseded,
        ] {
            let mut record = record();
            record.state = state;
            record.stage = "accepted-or-opaque-history".into();
            assert_eq!(
                ComputeFailureDisposition::decide(&record, false).unwrap(),
                ComputeFailureDisposition::Preserve
            );
        }
    }

    #[test]
    fn reporting_error_keeps_the_original_typed_cause_and_secondary_failure() {
        let original = container_runtime_api::ContainerRuntimeError::K8sError(
            "original-runtime-failure".into(),
        );
        let combined = with_reporting_error(original.into(), Err(anyhow!("second-store-failure")));
        assert!(
            combined
                .downcast_ref::<container_runtime_api::ContainerRuntimeError>()
                .is_some()
        );
        let chain = format!("{combined:#}");
        assert!(chain.contains("original-runtime-failure"));
        assert!(chain.contains("second-store-failure"));
    }
}
