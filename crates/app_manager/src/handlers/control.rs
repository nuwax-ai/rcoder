//! Durable operation queries and explicit identity recreation.
use super::AppManagerState;
use crate::models::OwnerParams;
use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
};
use shared_types::{
    AppError, HttpResult, UserAppLifecycleRecord, UserAppOperationView, UserAppRecreateRequest,
};
use std::sync::Arc;

/// An error after admission must remain correlated with the durable operation.
/// A failed diagnostic lookup never replaces the original business failure.
pub(super) async fn control_result<T>(
    state: &AppManagerState,
    app_id: &str,
    owner: &str,
    request_id: &str,
    result: crate::models::AppResult<T>,
) -> Result<T, AppError> {
    let error = match result {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    let error: AppError = error.into();
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        state
            .app_service
            .get_control_operation_by_request(app_id, owner, request_id),
    )
    .await
    {
        Ok(Ok(Some(operation))) => Err(error.with_operation_id(operation.operation_id)),
        Ok(Ok(None)) => Err(error),
        Ok(Err(lookup)) => {
            tracing::warn!(%app_id, %request_id, %lookup, "Failed to correlate control error with its operation");
            Err(error)
        }
        Err(_) => {
            tracing::warn!(%app_id, %request_id, "Control error correlation lookup timed out");
            Err(error)
        }
    }
}

fn owner(query: Result<Query<OwnerParams>, QueryRejection>) -> Result<String, AppError> {
    let Query(query) = query
        .map_err(|_| AppError::validation_error("A valid user_id query parameter is required"))?;
    shared_types::validate_identifier(&query.user_id, "user_id")
        .map_err(|_| AppError::validation_error("Invalid user_id query parameter"))?;
    Ok(query.user_id)
}

/// Retry or reconcile an application operation.
///
/// Pending commands retain their original input and identity. Running or uncertain
/// operations require durable final evidence; unknown remote effects are not replayed.
#[utoipa::path(post, path="/api/v1/userapp/{app_id}/operations/{operation_id}/retry",
    params(("app_id"=String, Path, description="Application identifier"), ("operation_id"=String, Path, description="Durable operation identifier")),
    request_body=shared_types::UserAppRetryRequest,
    responses((status=200, description="HttpResult envelope with the durable state or business error code", body=HttpResult<UserAppOperationView>)), tag="Userapp · 双态 · 生命周期")]
pub async fn retry_operation(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, operation_id)): Path<(String, String)>,
    body: Result<Json<shared_types::UserAppRetryRequest>, JsonRejection>,
) -> Result<Json<HttpResult<UserAppOperationView>>, AppError> {
    let Json(request) = body
        .map_err(|_| AppError::validation_error("A valid operation retry JSON body is required"))?;
    let service = state.app_service.clone();
    // Once retry begins, dropping the HTTP response observer must not cancel an
    // execution that may already have acquired a durable claim.
    let result = tokio::spawn(async move {
        service
            .retry_control_operation(&app_id, &operation_id, request)
            .await
    })
    .await
    .map_err(|error| {
        tracing::error!(%error, "Operation retry worker interrupted");
        AppError::internal_server_error(
            "Operation retry worker interrupted; query the operation before retrying",
        )
    })??;
    let id = result.operation_id.clone();
    Ok(Json(HttpResult::success(result).with_operation_id(id)))
}

/// Read application lifecycle.
#[utoipa::path(get, path="/api/v1/userapp/{app_id}/lifecycle",
    params(("app_id"=String, Path, description="Application identifier"), ("user_id"=String, Query, description="Application owner identifier")),
    responses((status=200, description="HttpResult envelope with the durable state or business error code", body=HttpResult<UserAppLifecycleRecord>)), tag="Userapp · 双态 · 生命周期")]
pub async fn get_lifecycle(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    query: Result<Query<OwnerParams>, QueryRejection>,
) -> Result<Json<HttpResult<UserAppLifecycleRecord>>, AppError> {
    let owner = owner(query)?;
    Ok(Json(HttpResult::success(
        state.app_service.get_lifecycle(&app_id, &owner).await?,
    )))
}

/// Read the current application operation.
#[utoipa::path(get, path="/api/v1/userapp/{app_id}/operations/current",
    params(("app_id"=String, Path, description="Application identifier"), ("user_id"=String, Query, description="Application owner identifier")),
    responses((status=200, description="HttpResult envelope with the durable state or business error code", body=HttpResult<Option<UserAppOperationView>>)), tag="Userapp · 双态 · 生命周期")]
pub async fn get_current_operation(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    query: Result<Query<OwnerParams>, QueryRejection>,
) -> Result<Json<HttpResult<Option<UserAppOperationView>>>, AppError> {
    let owner = owner(query)?;
    Ok(Json(HttpResult::success(
        state
            .app_service
            .get_control_operation(&app_id, &owner, None)
            .await?,
    )))
}

/// Read an application operation.
#[utoipa::path(get, path="/api/v1/userapp/{app_id}/operations/{operation_id}",
    params(("app_id"=String, Path, description="Application identifier"), ("operation_id"=String, Path, description="Durable operation identifier"), ("user_id"=String, Query, description="Application owner identifier")),
    responses((status=200, description="HttpResult envelope with the durable state or business error code", body=HttpResult<Option<UserAppOperationView>>)), tag="Userapp · 双态 · 生命周期")]
pub async fn get_operation(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, operation_id)): Path<(String, String)>,
    query: Result<Query<OwnerParams>, QueryRejection>,
) -> Result<Json<HttpResult<Option<UserAppOperationView>>>, AppError> {
    let owner = owner(query)?;
    Ok(Json(HttpResult::success(
        state
            .app_service
            .get_control_operation(&app_id, &owner, Some(&operation_id))
            .await?,
    )))
}

#[derive(serde::Deserialize)]
pub struct OperationRequestQuery {
    pub user_id: String,
    pub request_id: String,
}

/// Find an operation by request identity.
#[utoipa::path(get, path="/api/v1/userapp/{app_id}/operations/by-request",
    params(("app_id"=String, Path, description="Application identifier"), ("user_id"=String, Query, description="Application owner identifier"), ("request_id"=String, Query, description="Original request deduplication identifier")),
    responses((status=200, description="HttpResult envelope with the durable state or business error code", body=HttpResult<Option<UserAppOperationView>>)), tag="Userapp · 双态 · 生命周期")]
pub async fn get_operation_by_request(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    query: Result<Query<OperationRequestQuery>, QueryRejection>,
) -> Result<Json<HttpResult<Option<UserAppOperationView>>>, AppError> {
    let Query(query) = query.map_err(|_| {
        AppError::validation_error("Valid user_id and request_id query parameters are required")
    })?;
    shared_types::validate_identifier(&query.user_id, "user_id")
        .map_err(|_| AppError::validation_error("Invalid user_id query parameter"))?;
    let operation = state
        .app_service
        .get_control_operation_by_request(&app_id, &query.user_id, &query.request_id)
        .await?;
    Ok(Json(HttpResult::success(operation)))
}

/// Recreate a deleted application identity.
#[utoipa::path(post, path="/api/v1/userapp/{app_id}/recreate",
    params(("app_id"=String, Path, description="Application identifier")), request_body=UserAppRecreateRequest,
    responses((status=200, description="HttpResult envelope with the durable state or business error code", body=HttpResult<UserAppLifecycleRecord>)), tag="Userapp · 双态 · 生命周期")]
pub async fn recreate_identity(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    body: Result<Json<UserAppRecreateRequest>, JsonRejection>,
) -> Result<Json<HttpResult<UserAppLifecycleRecord>>, AppError> {
    let Json(body) =
        body.map_err(|_| AppError::validation_error("A valid recreation JSON body is required"))?;
    Ok(Json(HttpResult::success(
        state.app_service.recreate_identity(&app_id, body).await?,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn control_failure_correlation_requires_matching_persisted_owner() {
        let directory = tempfile::tempdir().expect("test directory");
        let service = Arc::new(
            crate::test_support::test_service(
                directory.path(),
                Arc::new(crate::test_support::MockRuntime::default()),
            )
            .await,
        );
        service
            .metadata
            .store
            .ensure_identity("correlation", "owner")
            .await
            .expect("identity");
        service
            .metadata
            .store
            .admit(&shared_types::UserAppAdmission {
                runtime_policy_on_success: None,
                app_id: "correlation".into(),
                user_id: "owner".into(),
                lifecycle_id: None,
                operation_id: "persisted-operation".into(),
                request_id: Some("caller-request".into()),
                request_fingerprint: "a".repeat(64),
                kind: shared_types::UserAppOperationKind::Stop,
                command: Some(shared_types::UserAppControlCommand::Stop {
                    wake_on_traffic: false,
                }),
                metadata: None,
            })
            .await
            .expect("admission");
        let state = AppManagerState {
            app_service: service,
            http_client: reqwest::Client::new(),
        };
        for (owner, request, expected) in [
            ("owner", "caller-request", Some("persisted-operation")),
            ("other-owner", "caller-request", None),
            ("owner", "unaccepted-request", None),
        ] {
            let result: crate::models::AppResult<()> = Err(
                crate::models::AppOperationError::Backend("Original runtime failure".into()),
            );
            let error = control_result(&state, "correlation", owner, request, result)
                .await
                .expect_err("original failure");
            match error {
                AppError::Structured {
                    code,
                    internal_message,
                    operation_id,
                    ..
                } => {
                    assert_eq!(code, shared_types::error_codes::ERR_BACKEND_ERROR);
                    assert_eq!(
                        internal_message.as_deref(),
                        Some("Original runtime failure")
                    );
                    assert_eq!(operation_id.as_deref(), expected);
                }
                other => panic!("Expected structured error: {other:?}"),
            }
        }
    }
}

#[cfg(test)]
mod builder_retry_tests {
    use super::*;
    use shared_types::{
        UserAppAdmission, UserAppAdmissionOutcome, UserAppBuilderRecovery, UserAppOperationKind,
        UserAppOperationRecord,
    };
    use std::sync::Mutex;

    struct RecordingRecovery(
        Mutex<Vec<UserAppOperationRecord>>,
        std::sync::atomic::AtomicBool,
    );
    #[async_trait::async_trait]
    impl UserAppBuilderRecovery for RecordingRecovery {
        async fn resume_pending(&self, record: &UserAppOperationRecord) -> Result<bool, String> {
            assert_eq!(record.state, shared_types::UserAppOperationState::Pending);
            self.0
                .lock()
                .map_err(|_| "Test recorder poisoned")?
                .push(record.clone());
            Ok(self.1.load(std::sync::atomic::Ordering::SeqCst))
        }
        async fn reconcile_completed(
            &self,
            record: &UserAppOperationRecord,
        ) -> Result<bool, String> {
            assert!(shared_types::userapp_operation_has_final_evidence(record));
            assert_ne!(record.state, shared_types::UserAppOperationState::Pending);
            self.0
                .lock()
                .map_err(|_| "Test recorder poisoned")?
                .push(record.clone());
            Ok(true)
        }
    }

    #[tokio::test]
    async fn retry_handler_separates_pending_execution_from_confirmed_builder_reconciliation() {
        for kind in [
            UserAppOperationKind::EnsureBuilder,
            UserAppOperationKind::StopBuilder,
            UserAppOperationKind::RestartBuilder,
        ] {
            let root = tempfile::tempdir().unwrap();
            let service = crate::test_support::test_service(
                root.path(),
                Arc::new(crate::test_support::MockRuntime::default()),
            )
            .await;
            let recorder = Arc::new(RecordingRecovery(
                Mutex::new(Vec::new()),
                std::sync::atomic::AtomicBool::new(true),
            ));
            service.set_builder_recovery(recorder.clone()).unwrap();
            let store = service.metadata.store.clone();
            let command = match kind {
                UserAppOperationKind::StopBuilder => {
                    Some(shared_types::UserAppControlCommand::StopBuilder)
                }
                UserAppOperationKind::RestartBuilder => {
                    Some(shared_types::UserAppControlCommand::RestartBuilder)
                }
                _ => None,
            };
            let admitted = store
                .admit(&UserAppAdmission {
                    app_id: "retry-builder".into(),
                    user_id: "owner".into(),
                    lifecycle_id: None,
                    operation_id: "original-operation".into(),
                    request_id: Some("original-request".into()),
                    request_fingerprint: "a".repeat(64),
                    kind,
                    command,
                    metadata: None,
                    runtime_policy_on_success: None,
                })
                .await
                .unwrap();
            let record = match admitted {
                UserAppAdmissionOutcome::Accepted(record)
                | UserAppAdmissionOutcome::Existing(record) => record,
            };
            let request = shared_types::UserAppRetryRequest {
                user_id: "owner".into(),
                lifecycle_id: record.lifecycle_id.clone(),
                expected_revision: record.revision,
            };
            let state = Arc::new(AppManagerState {
                app_service: Arc::new(service),
                http_client: reqwest::Client::new(),
            });
            for invalid in [
                shared_types::UserAppRetryRequest {
                    user_id: "other-owner".into(),
                    ..request.clone()
                },
                shared_types::UserAppRetryRequest {
                    lifecycle_id: "old-lifecycle".into(),
                    ..request.clone()
                },
                shared_types::UserAppRetryRequest {
                    expected_revision: request.expected_revision + 1,
                    ..request.clone()
                },
            ] {
                assert!(
                    retry_operation(
                        State(state.clone()),
                        Path((record.app_id.clone(), record.operation_id.clone())),
                        Ok(Json(invalid))
                    )
                    .await
                    .is_err()
                );
                assert!(recorder.0.lock().unwrap().is_empty());
            }
            let result = retry_operation(
                State(state.clone()),
                Path((record.app_id.clone(), record.operation_id.clone())),
                Ok(Json(request.clone())),
            )
            .await
            .unwrap();
            assert_eq!(
                result.0.operation_id.as_deref(),
                Some(record.operation_id.as_str())
            );
            assert_eq!(
                recorder.0.lock().unwrap().as_slice(),
                std::slice::from_ref(&record)
            );
            recorder.1.store(false, std::sync::atomic::Ordering::SeqCst);
            assert!(
                retry_operation(
                    State(state.clone()),
                    Path((record.app_id.clone(), record.operation_id.clone())),
                    Ok(Json(request.clone()))
                )
                .await
                .is_err(),
                "unchanged Pending operation must not be reported as resumed"
            );
            recorder.1.store(true, std::sync::atomic::Ordering::SeqCst);
            // An uncertain in-flight builder is never sent to a Pending kernel.
            let running = store
                .advance(&shared_types::UserAppOperationProgress {
                    app_id: record.app_id.clone(),
                    operation_id: record.operation_id.clone(),
                    lifecycle_id: record.lifecycle_id.clone(),
                    expected_revision: record.revision,
                    executor_id: "worker".into(),
                    state: shared_types::UserAppOperationState::Running,
                    step: "claimed".into(),
                    checkpoint: serde_json::Value::Null,
                    error_code: None,
                    error_message: None,
                })
                .await
                .unwrap();
            assert!(
                retry_operation(
                    State(state.clone()),
                    Path((record.app_id.clone(), record.operation_id.clone())),
                    Ok(Json(shared_types::UserAppRetryRequest {
                        expected_revision: running.revision,
                        ..request.clone()
                    }))
                )
                .await
                .is_err()
            );
            assert_eq!(recorder.0.lock().unwrap().len(), 2);
            let context = shared_types::UserAppExecutionContext {
                app_id: record.app_id.clone(),
                user_id: "owner".into(),
                lifecycle_id: record.lifecycle_id.clone(),
                operation_id: record.operation_id.clone(),
                executor_id: "worker".into(),
                request_fingerprint: record.request_fingerprint.clone(),
            };
            let (step, checkpoint) = if kind == UserAppOperationKind::EnsureBuilder {
                (
                    "builder_ready_confirmed",
                    serde_json::to_value(shared_types::BuilderCreationEvidence {
                        creation_lease_released: true,
                        target: shared_types::BuilderControlTarget {
                            context: context.clone(),
                            resource_binding: None,
                            pod: None,
                            workload: Some(shared_types::AppResourceIdentity {
                                kind: shared_types::AppResourceKind::Container,
                                name: "builder".into(),
                                uid: "physical".into(),
                                resource_version: None,
                            }),
                        },
                        container: shared_types::ContainerBasicInfo {
                            container_id: "physical".into(),
                            container_name: "builder".into(),
                            container_ip: "127.0.0.1".into(),
                            internal_port: 60000,
                            external_port: 60000,
                            project_id: record.app_id.clone(),
                            status: "running".into(),
                            created_at: chrono::Utc::now(),
                            service_url: "http://127.0.0.1:60000".into(),
                        },
                    })
                    .unwrap(),
                )
            } else {
                (
                    "compute_confirmed",
                    serde_json::json!({"target":{"context":context},"result":{"operation_id":record.operation_id}}),
                )
            };
            let confirmed = store
                .advance(&shared_types::UserAppOperationProgress {
                    app_id: record.app_id.clone(),
                    operation_id: record.operation_id.clone(),
                    lifecycle_id: record.lifecycle_id.clone(),
                    expected_revision: running.revision,
                    executor_id: "worker".into(),
                    state: shared_types::UserAppOperationState::Running,
                    step: step.into(),
                    checkpoint,
                    error_code: None,
                    error_message: None,
                })
                .await
                .unwrap();
            assert!(shared_types::userapp_operation_has_final_evidence(
                &confirmed
            ));
            let reconciled = retry_operation(
                State(state),
                Path((record.app_id.clone(), record.operation_id.clone())),
                Ok(Json(shared_types::UserAppRetryRequest {
                    expected_revision: confirmed.revision,
                    ..request
                })),
            )
            .await
            .unwrap();
            assert_eq!(
                reconciled.0.operation_id.as_deref(),
                Some(record.operation_id.as_str())
            );
            assert_eq!(
                recorder.0.lock().unwrap().as_slice(),
                &[record.clone(), record, confirmed]
            );
        }
    }
}
