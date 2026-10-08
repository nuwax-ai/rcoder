//! Deterministic pre-admission deadline counterexamples. Each fixture uses the
//! real Turso ledger; physical lease outcomes remain component-test fixtures.
use super::super::restart_wait;
use super::*;

#[derive(Clone, Copy, Debug)]
enum DiagnosticDeadlineExit {
    Read,
    NextAttempt,
    Admission,
}

#[tokio::test]
async fn restart_deadline_preserves_verified_holder_across_read_and_attempt_boundaries() {
    for independent_service in [false, true] {
        for exit in [
            DiagnosticDeadlineExit::Read,
            DiagnosticDeadlineExit::NextAttempt,
            DiagnosticDeadlineExit::Admission,
        ] {
            let directory = tempfile::tempdir().unwrap();
            let app_id = "restartdiagnostic";
            let request_id = "bounded-restart-not-admitted";
            let (mut first, runtime) = created_service(directory.path(), app_id, 1).await;
            first.config.access_mode = AppAccessMode::Kubernetes;
            let second = AppService::new(
                first.config.clone(),
                runtime.clone(),
                Arc::new(crate::activity_registry::AppActivityRegistry::new(
                    Duration::from_secs(300),
                )),
                None,
                first.metadata.store.clone(),
            )
            .await
            .unwrap();
            let service = if independent_service { &second } else { &first };
            let holder = operation_holder(
                &first,
                app_id,
                UserAppOperationKind::HotDeploy,
                None,
                "hot_deploy_preparing",
            )
            .await;
            let admission = Arc::new(restart_wait::RestartAdmission::default());
            let error = admission
                .scope(async {
                    let deadline = service.restart_admission_deadline().unwrap();
                    let initial = match service.try_acquire_process_release_lock(app_id).await {
                        Ok(_) => panic!("the original holder must still own its lease"),
                        Err(error) => error,
                    };
                    assert_eq!(
                        initial
                            .operation_in_progress_data()
                            .unwrap()
                            .holder_operation_id
                            .as_deref(),
                        Some(holder.operation_id.as_str())
                    );
                    match exit {
                        DiagnosticDeadlineExit::Read => {
                            // The next read has entered but cannot finish. Its timeout is
                            // not evidence that the fully verified holder disappeared.
                            restart_wait::prepare(std::future::pending::<
                                Result<(), AppOperationError>,
                            >())
                            .await
                            .unwrap_err()
                        }
                        DiagnosticDeadlineExit::NextAttempt => {
                            tokio::time::sleep_until(deadline).await;
                            match service
                                .acquire_restart_admission_guard(app_id, None, deadline)
                                .await
                            {
                                Ok(_) => panic!("an exhausted request cannot be admitted"),
                                Err(error) => error,
                            }
                        }
                        DiagnosticDeadlineExit::Admission => {
                            tokio::time::sleep_until(deadline).await;
                            restart_wait::begin_admission().unwrap_err()
                        }
                    }
                })
                .await;
            assert_eq!(error.code(), shared_types::ERR_OPERATION_IN_PROGRESS);
            assert!(error.operation_id().is_none());
            let data = error.operation_in_progress_data().unwrap();
            assert_eq!(
                data.holder_operation_id.as_deref(),
                Some(holder.operation_id.as_str()),
                "deadline {exit:?}, independent_service={independent_service} must retain verified diagnostic identity"
            );
            assert_eq!(data.holder_kind.as_deref(), Some("hot_deploy"));
            assert_eq!(data.holder_state.as_deref(), Some("running"));
            assert!(data.retryable);
            assert_eq!(data.retry_after_seconds, 45);
            assert_not_admitted(service, app_id, request_id).await;
            assert_no_restart_mutation(&runtime);
            holder.finish().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_not_admitted(service, app_id, request_id).await;
            assert_no_restart_mutation(&runtime);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum InvalidatedHolder {
    Completed,
    Changed,
    ForeignReceipt,
    LifecycleMismatch,
    ObservationFailed,
}

#[derive(Clone, Copy, Debug)]
enum PartialHolderFact {
    RecordChanged,
    RevisionTimestampMissing,
    ComputeHeadChanged,
}

#[tokio::test]
async fn restart_new_durable_facts_invalidate_history_before_the_next_read_can_finish() {
    for fact in [
        PartialHolderFact::RecordChanged,
        PartialHolderFact::RevisionTimestampMissing,
        PartialHolderFact::ComputeHeadChanged,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let app_id = "restartpartialfact";
        let (mut service, runtime) = created_service(directory.path(), app_id, 1).await;
        service.config.access_mode = AppAccessMode::Kubernetes;
        let mut holder = operation_holder(
            &service,
            app_id,
            UserAppOperationKind::HotDeploy,
            None,
            "hot_deploy_preparing",
        )
        .await;
        let original = service
            .metadata
            .store
            .get_operation(app_id, &holder.operation_id)
            .await
            .unwrap()
            .unwrap();
        let error = Arc::new(restart_wait::RestartAdmission::default())
            .scope(async {
                let deadline = service.restart_admission_deadline().unwrap();
                let initial = service.prod_lock_conflict(app_id).await;
                assert_eq!(
                    initial
                        .operation_in_progress_data()
                        .unwrap()
                        .retry_after_seconds,
                    45
                );
                match fact {
                    PartialHolderFact::RecordChanged
                    | PartialHolderFact::RevisionTimestampMissing => {
                        holder
                            .operation
                            .checkpoint("hot_deploy_observing", serde_json::json!({}))
                            .await
                            .unwrap();
                        match fact {
                            PartialHolderFact::RecordChanged => {
                                let record = service
                                    .metadata
                                    .store
                                    .get_operation(app_id, &holder.operation_id)
                                    .await
                                    .unwrap()
                                    .unwrap();
                                assert!(record.revision > original.revision);
                                restart_wait::observe_record(
                                    &record.app_id,
                                    &record.lifecycle_id,
                                    &record.blocker(),
                                    record.revision,
                                )
                                .unwrap();
                            }
                            PartialHolderFact::RevisionTimestampMissing => {
                                let updated = service
                                    .metadata
                                    .store
                                    .get_operation_updated_at(
                                        app_id,
                                        &holder.operation_id,
                                        original.revision,
                                    )
                                    .await
                                    .unwrap();
                                assert!(updated.is_none());
                                restart_wait::observe_updated_at(updated).unwrap();
                            }
                            PartialHolderFact::ComputeHeadChanged => unreachable!(),
                        }
                    }
                    PartialHolderFact::ComputeHeadChanged => {
                        let control = service
                            .metadata
                            .store
                            .admit_compute_control(&ComputeControlRequest {
                                app_id: app_id.into(),
                                lifecycle_id: original.lifecycle_id.clone(),
                                scope: UserAppOperationScope::Prod,
                                operation_id: uuid::Uuid::new_v4().to_string(),
                                request_id: "new-physical-stop-head".into(),
                                request_fingerprint: "f".repeat(64),
                                action: ComputeControlAction::Stop,
                                restart_image_roll: false,
                            })
                            .await
                            .unwrap();
                        let blocker = super::super::operation_progress::compute_blocker(&control);
                        tokio::time::sleep_until(deadline).await;
                        // Constructing a diagnostic future consumes the already-read
                        // head change even when the total budget forbids its first poll.
                        return restart_wait::bounded_read(
                            deadline,
                            service.observe_operation_holder(
                                app_id,
                                UserAppOperationScope::Prod,
                                Some(&blocker),
                                None,
                            ),
                        )
                        .await
                        .err()
                        .unwrap();
                    }
                }
                restart_wait::prepare(std::future::pending::<Result<(), AppOperationError>>())
                    .await
                    .unwrap_err()
            })
            .await;
        let data = error.operation_in_progress_data().unwrap();
        assert!(data.holder_operation_id.is_none(), "partial fact {fact:?}");
        assert!(!data.retryable);
        assert_eq!(data.retry_after_seconds, 0);
        assert_not_admitted(&service, app_id, "partial-fact-not-admitted").await;
        assert_no_restart_mutation(&runtime);
        if matches!(fact, PartialHolderFact::ComputeHeadChanged) {
            holder.guard.take().unwrap().finish().await.unwrap();
        } else {
            holder.finish().await;
        }
    }
}

#[tokio::test]
async fn restart_deadline_never_reuses_diagnostics_after_a_completed_negative_observation() {
    for invalidation in [
        InvalidatedHolder::Completed,
        InvalidatedHolder::Changed,
        InvalidatedHolder::ForeignReceipt,
        InvalidatedHolder::LifecycleMismatch,
        InvalidatedHolder::ObservationFailed,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let app_id = "restartinvalidate";
        let request_id = "invalidated-wait-not-admitted";
        let (mut service, runtime) = created_service(directory.path(), app_id, 1).await;
        service.config.access_mode = AppAccessMode::Kubernetes;
        let holder = traffic_holder(&service, app_id).await;
        let original = service
            .metadata
            .store
            .get_operation(app_id, &holder.operation_id)
            .await
            .unwrap()
            .unwrap();
        let mut holder_operation = Some(holder.operation);
        let mut replacement = None;
        let admission = Arc::new(restart_wait::RestartAdmission::default());
        let error = admission
            .scope(async {
                let deadline = service.restart_admission_deadline().unwrap();
                let initial = service.prod_lock_conflict(app_id).await;
                assert_traffic_diagnostic(
                    &initial.operation_in_progress_data().unwrap(),
                    &holder.operation_id,
                );
                match invalidation {
                    InvalidatedHolder::Completed | InvalidatedHolder::Changed => {
                        holder_operation.take().unwrap().succeed().await.unwrap();
                        if matches!(invalidation, InvalidatedHolder::Changed) {
                            replacement = Some(
                                OwnedOperation::admit(
                                    service.metadata.store.clone(),
                                    UserAppAdmission {
                                        runtime_policy_on_success: None,
                                        command: Some(UserAppControlCommand::Start {
                                            traffic: true,
                                        }),
                                        app_id: app_id.into(),
                                        lifecycle_id: Some(original.lifecycle_id.clone()),
                                        operation_id: uuid::Uuid::new_v4().to_string(),
                                        request_id: Some("replacement-holder".into()),
                                        request_fingerprint: "e".repeat(64),
                                        kind: UserAppOperationKind::Start,
                                        metadata: None,
                                    },
                                )
                                .await
                                .unwrap(),
                            );
                        }
                        let observed = service
                            .observe_operation_holder(
                                app_id,
                                UserAppOperationScope::Prod,
                                Some(&original.blocker()),
                                None,
                            )
                            .await
                            .unwrap();
                        assert!(observed.blocker.is_none());
                    }
                    InvalidatedHolder::ForeignReceipt | InvalidatedHolder::ObservationFailed => {
                        runtime.lease_validation_fails.store(
                            matches!(invalidation, InvalidatedHolder::ObservationFailed),
                            Ordering::SeqCst,
                        );
                        let physical = shared_types::UserAppOperationInProgress {
                            app_id: app_id.into(),
                            service_type: shared_types::ServiceType::Userapp,
                            resource_name: format!("rcoder-operation-prod-{app_id}"),
                            operation_id: matches!(invalidation, InvalidatedHolder::ForeignReceipt)
                                .then(|| "foreign-lease-token".into()),
                        };
                        let observed = service
                            .observe_operation_holder(
                                app_id,
                                UserAppOperationScope::Prod,
                                None,
                                Some(&physical),
                            )
                            .await;
                        match invalidation {
                            InvalidatedHolder::ForeignReceipt => {
                                assert!(observed.unwrap().blocker.is_none());
                            }
                            InvalidatedHolder::ObservationFailed => {
                                let error = observed.err().unwrap();
                                assert_eq!(error.code(), shared_types::ERR_RUNTIME_UNAVAILABLE);
                                assert!(
                                    error.message().contains("observation endpoint unavailable")
                                );
                                assert!(error.operation_in_progress_data().is_none());
                            }
                            _ => unreachable!(),
                        }
                    }
                    InvalidatedHolder::LifecycleMismatch => {
                        let error = restart_wait::bounded_read(
                            deadline,
                            service.metadata.validate_request_lifecycle(
                                app_id,
                                Some("replaced-application-lifecycle"),
                            ),
                        )
                        .await
                        .unwrap_err();
                        assert_eq!(error.code(), shared_types::ERR_CONFLICT);
                        assert!(error.operation_in_progress_data().is_none());
                    }
                }
                restart_wait::prepare(std::future::pending::<Result<(), AppOperationError>>())
                    .await
                    .unwrap_err()
            })
            .await;
        assert_eq!(error.code(), shared_types::ERR_OPERATION_IN_PROGRESS);
        let data = error.operation_in_progress_data().unwrap();
        assert!(
            data.holder_operation_id.is_none(),
            "{invalidation:?} must supersede the earlier known holder"
        );
        assert!(!data.retryable);
        assert_eq!(data.retry_after_seconds, 0);
        assert_not_admitted(&service, app_id, request_id).await;
        assert_no_restart_mutation(&runtime);
        if let Some(operation) = holder_operation {
            operation.succeed().await.unwrap();
        }
        if let Some(operation) = replacement {
            operation.succeed().await.unwrap();
        }
        holder.guard.unwrap().finish().await.unwrap();
    }
}

#[tokio::test]
async fn restart_new_physical_identity_invalidates_history_before_a_budget_exhausted_read() {
    for change in ["same", "application", "scope", "resource", "token"] {
        let directory = tempfile::tempdir().unwrap();
        let app_id = "restartphysicalchange";
        let (mut first, runtime) = created_service(directory.path(), app_id, 1).await;
        first.config.access_mode = AppAccessMode::Kubernetes;
        let second = AppService::new(
            first.config.clone(),
            runtime.clone(),
            Arc::new(crate::activity_registry::AppActivityRegistry::new(
                Duration::from_secs(300),
            )),
            None,
            first.metadata.store.clone(),
        )
        .await
        .unwrap();
        let holder = operation_holder(
            &first,
            app_id,
            UserAppOperationKind::HotDeploy,
            None,
            "hot_deploy_preparing",
        )
        .await;
        let admission = Arc::new(restart_wait::RestartAdmission::default());
        let error = admission
            .scope(async {
                let deadline = second.restart_admission_deadline().unwrap();
                let verified = match second.try_acquire_process_release_lock(app_id).await {
                    Ok(_) => panic!("the physical holder must remain occupied"),
                    Err(error) => error,
                };
                assert_eq!(
                    verified
                        .operation_in_progress_data()
                        .unwrap()
                        .holder_operation_id
                        .as_deref(),
                    Some(holder.operation_id.as_str())
                );
                tokio::time::sleep_until(deadline).await;
                let mut incoming = shared_types::UserAppOperationInProgress {
                    app_id: app_id.into(),
                    service_type: shared_types::ServiceType::Userapp,
                    resource_name: format!("rcoder-operation-prod-{app_id}"),
                    operation_id: None,
                };
                match change {
                    "same" => {}
                    "application" => incoming.app_id = "foreign-app".into(),
                    "scope" => incoming.service_type = shared_types::ServiceType::UserappBuilder,
                    "resource" => incoming.resource_name = "foreign-resource".into(),
                    "token" => incoming.operation_id = Some("new-physical-holder".into()),
                    _ => unreachable!(),
                }
                restart_wait::observe_physical_conflict(
                    app_id,
                    UserAppOperationScope::Prod,
                    &incoming,
                )
                .unwrap();
                let read_started = std::sync::atomic::AtomicBool::new(false);
                let error = restart_wait::bounded_diagnostic(deadline, async {
                    read_started.store(true, Ordering::SeqCst);
                    std::future::pending::<AppOperationError>().await
                })
                .await;
                assert!(!read_started.load(Ordering::SeqCst));
                error
            })
            .await;
        let data = error.operation_in_progress_data().unwrap();
        if change == "same" {
            assert_eq!(
                data.holder_operation_id.as_deref(),
                Some(holder.operation_id.as_str())
            );
            assert!(data.retryable);
            assert_eq!(data.retry_after_seconds, 45);
        } else {
            assert!(data.holder_operation_id.is_none(), "new {change} fact");
            assert!(!data.retryable);
            assert_eq!(data.retry_after_seconds, 0);
        }
        assert_not_admitted(&second, app_id, "physical-changed-not-admitted").await;
        assert_no_restart_mutation(&runtime);
        holder.finish().await;
    }
}
