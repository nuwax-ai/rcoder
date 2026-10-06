//! Diagnostics derived from runtime variants and dispatch evidence, never text.
use crate::ContainerRuntimeError;
use shared_types::error_codes::*;
use shared_types::{
    AppError, ErrorDetail, PgCommandDiagnostic, PgCommandError, PgCommandEvidence, PgCommandMode,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeErrorContext {
    ReadOnly,
    Mutation,
}

/// The second element is a safe summary for credential-bearing exec paths.
pub fn runtime_error_code(error: &ContainerRuntimeError) -> (&'static str, &'static str) {
    match error {
        ContainerRuntimeError::ConfigurationError(_) => (
            ERR_RUNTIME_CONFIGURATION,
            "Runtime configuration is invalid",
        ),
        ContainerRuntimeError::ConnectionError(_) | ContainerRuntimeError::ManagementNotRunning => {
            (ERR_RUNTIME_UNAVAILABLE, "Runtime management is unavailable")
        }
        ContainerRuntimeError::Timeout(_) => (ERR_RUNTIME_TIMEOUT, "Runtime request timed out"),
        ContainerRuntimeError::ContainerCreationError(_) => {
            (ERR_CONTAINER_CREATE_FAILED, "Container creation failed")
        }
        ContainerRuntimeError::ContainerStartError(_) => {
            (ERR_CONTAINER_START_FAILED, "Container start failed")
        }
        ContainerRuntimeError::ContainerStopError(_) => {
            (ERR_CONTAINER_STOP_FAILED, "Container stop failed")
        }
        ContainerRuntimeError::ContainerExecError(_) => {
            (ERR_CONTAINER_EXEC_FAILED, "Container command failed")
        }
        ContainerRuntimeError::ContainerNotFound(_) => {
            (ERR_CONTAINER_NOT_FOUND, "Container does not exist")
        }
        ContainerRuntimeError::Conflict(_)
        | ContainerRuntimeError::OperationInProgress(_)
        | ContainerRuntimeError::CreationCancelled => (
            ERR_CONFLICT,
            "A conflicting runtime operation prevents this request",
        ),
        ContainerRuntimeError::PreparationFailed(_) => {
            (ERR_BACKEND_ERROR, "Application preparation failed")
        }
        ContainerRuntimeError::RequestRejected(rejection) if rejection.status == 409 => {
            (ERR_CONFLICT, "Runtime rejected a conflicting request")
        }
        ContainerRuntimeError::RequestRejected(_) => {
            (ERR_BACKEND_ERROR, "Runtime rejected the request")
        }
        ContainerRuntimeError::CreationAborted { source, .. } => runtime_error_code(source),
        ContainerRuntimeError::K8sError(_) | ContainerRuntimeError::DockerError(_) => {
            (ERR_CONTAINER_ERROR, "Unclassified runtime failure")
        }
    }
}

/// Whether a dispatched runtime mutation lacks definitive completion evidence.
/// Read-only callers must not use this to infer that any write occurred.
pub fn runtime_mutation_outcome_unknown(error: &ContainerRuntimeError) -> bool {
    match error {
        ContainerRuntimeError::CreationAborted { progress, .. } => !progress.safe_finish_ok(),
        ContainerRuntimeError::ConnectionError(_)
        | ContainerRuntimeError::Timeout(_)
        | ContainerRuntimeError::K8sError(_)
        | ContainerRuntimeError::DockerError(_) => true,
        ContainerRuntimeError::RequestRejected(rejection) => {
            !(400..500).contains(&rejection.status) || matches!(rejection.status, 408 | 499)
        }
        _ => false,
    }
}

pub fn runtime_app_error(
    error: &ContainerRuntimeError,
    stage: &str,
    context: RuntimeErrorContext,
) -> AppError {
    let (cause_code, _) = runtime_error_code(error);
    let unknown =
        context == RuntimeErrorContext::Mutation && runtime_mutation_outcome_unknown(error);
    let code = if unknown {
        ERR_OPERATION_OUTCOME_UNKNOWN
    } else {
        cause_code
    };
    let message = shared_types::sanitize_error_text(&format!("{stage}: {error}"));
    let retryable = context == RuntimeErrorContext::ReadOnly
        && matches!(cause_code, ERR_RUNTIME_UNAVAILABLE | ERR_RUNTIME_TIMEOUT);
    let mut result = AppError::with_message(code, message.clone())
        .with_error_detail(ErrorDetail::new(cause_code, stage, message).with_retryable(retryable));
    // A registration or a later observation must never fabricate an operation.
    let mut origin = error;
    while let ContainerRuntimeError::CreationAborted { source, .. } = origin {
        origin = source;
    }
    if let ContainerRuntimeError::OperationInProgress(operation) = origin
        && let Some(id) = &operation.operation_id
    {
        result = result.with_operation_id(id.clone());
    }
    result
}

/// Command text and raw transport messages can contain shell-escaped passwords.
/// Keep the typed cause, but expose a safe exec summary here.
pub fn runtime_pg_command_error(
    error: &ContainerRuntimeError,
    mode: PgCommandMode,
) -> PgCommandError {
    let (cause_code, summary) = runtime_error_code(error);
    let evidence = match error {
        ContainerRuntimeError::ConfigurationError(_)
        | ContainerRuntimeError::ContainerNotFound(_)
        | ContainerRuntimeError::Conflict(_)
        | ContainerRuntimeError::ManagementNotRunning
        | ContainerRuntimeError::OperationInProgress(_) => PgCommandEvidence::NotDispatched,
        ContainerRuntimeError::RequestRejected(rejection)
            if (400..500).contains(&rejection.status) && !matches!(rejection.status, 408 | 499) =>
        {
            PgCommandEvidence::DefinitivelyRejected
        }
        _ => PgCommandEvidence::OutcomeUnknown,
    };
    let code = if mode == PgCommandMode::Write && evidence == PgCommandEvidence::OutcomeUnknown {
        ERR_OPERATION_OUTCOME_UNKNOWN
    } else {
        cause_code
    };
    let mut result = PgCommandError::new(code, summary, evidence).with_cause_code(cause_code);
    let mut origin = error;
    while let ContainerRuntimeError::CreationAborted { source, .. } = origin {
        origin = source;
    }
    if let ContainerRuntimeError::OperationInProgress(operation) = origin
        && let Some(id) = &operation.operation_id
    {
        result = result.with_diagnostic(PgCommandDiagnostic {
            code: code.into(),
            operation_id: Some(id.clone()),
            blocker: None,
            error_detail: None,
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CreationProgress, CreationStage};

    #[test]
    fn typed_runtime_diagnostics_distinguish_read_timeout_and_unknown_write() {
        let error = ContainerRuntimeError::Timeout("lost reply".into());
        let read = runtime_app_error(&error, "query", RuntimeErrorContext::ReadOnly)
            .into_http_result::<()>("en-US");
        assert_eq!(read.code, ERR_RUNTIME_TIMEOUT);
        assert!(read.error_detail.unwrap().retryable);
        let write = runtime_app_error(&error, "stop", RuntimeErrorContext::Mutation)
            .into_http_result::<()>("en-US");
        assert_eq!(write.code, ERR_OPERATION_OUTCOME_UNKNOWN);
        assert!(!write.error_detail.unwrap().retryable);
    }

    #[test]
    fn creation_evidence_preserves_fence_and_original_phase() {
        for (stage, expected) in [
            (CreationStage::Capture, ERR_RUNTIME_TIMEOUT),
            (
                CreationStage::WriteGeneration,
                ERR_OPERATION_OUTCOME_UNKNOWN,
            ),
        ] {
            let error = ContainerRuntimeError::CreationAborted {
                progress: CreationProgress {
                    failed_at: stage,
                    ..Default::default()
                },
                source: Box::new(ContainerRuntimeError::Timeout("lost reply".into())),
            };
            let response = runtime_app_error(&error, "create", RuntimeErrorContext::Mutation)
                .into_http_result::<()>("en-US");
            assert_eq!(response.code, expected);
            assert!(!response.error_detail.unwrap().retryable);
        }
    }

    #[test]
    fn credential_exec_does_not_echo_shell_or_guess_dispatch() {
        let error = ContainerRuntimeError::ContainerExecError(
            "ALTER ROLE u PASSWORD 'private_marker'".into(),
        );
        let write = runtime_pg_command_error(&error, PgCommandMode::Write);
        assert_eq!(write.code, ERR_OPERATION_OUTCOME_UNKNOWN);
        assert_eq!(write.evidence, PgCommandEvidence::OutcomeUnknown);
        assert!(!write.detail.contains("private_marker"));
        let missing = runtime_pg_command_error(
            &ContainerRuntimeError::ContainerNotFound("gone".into()),
            PgCommandMode::Write,
        );
        assert_eq!(missing.code, ERR_CONTAINER_NOT_FOUND);
        assert_eq!(missing.evidence, PgCommandEvidence::NotDispatched);
    }

    #[test]
    fn credential_exec_conflict_keeps_original_operation_identity() {
        let original = ContainerRuntimeError::OperationInProgress(Box::new(
            shared_types::UserAppOperationInProgress {
                app_id: "fixtureapp".into(),
                service_type: shared_types::ServiceType::Userapp,
                resource_name: "fixture-operation".into(),
                operation_id: Some("original-runtime-exec".into()),
            },
        ));
        let command = runtime_pg_command_error(&original, PgCommandMode::Write);
        assert_eq!(command.code, ERR_CONFLICT);
        assert_eq!(command.evidence, PgCommandEvidence::NotDispatched);
        let response = command
            .into_app_error("database_prepare")
            .into_http_result::<()>("en-US");
        assert_eq!(response.code, ERR_CONFLICT);
        assert_eq!(
            response.operation_id.as_deref(),
            Some("original-runtime-exec"),
            "credential exec cannot discard the conflicting operation identity"
        );
        assert!(!response.error_detail.unwrap().retryable);
    }
}
