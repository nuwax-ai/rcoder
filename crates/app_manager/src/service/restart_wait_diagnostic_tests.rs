//! Deterministic pre-admission deadline counterexamples. Each fixture uses the
//! real Turso ledger; physical lease outcomes remain component-test fixtures.
use super::super::restart_wait;
use super::*;

#[derive(Clone, Copy, Debug)]
enum DiagnosticReleaseRace {
    Released,
    RecoveryRequired,
    ReplacementStop,
    ReplacementDelete,
    PriorityStop,
    TerminalStopIntent,
}

#[tokio::test]
async fn restart_rechecks_exact_rejected_holder_when_it_finishes_during_diagnostics() {
    for race in [
        DiagnosticReleaseRace::Released,
        DiagnosticReleaseRace::RecoveryRequired,
        DiagnosticReleaseRace::ReplacementStop,
        DiagnosticReleaseRace::ReplacementDelete,
        DiagnosticReleaseRace::PriorityStop,
        DiagnosticReleaseRace::TerminalStopIntent,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let app_id = "diagnosticrelease";
        let request_id = "restart-release-original";
        let (service, runtime) = created_service(directory.path(), app_id, 2).await;
        let mut holder = traffic_holder(&service, app_id).await;
        holder.release_physical_guard().await;
        let original = service
            .metadata
            .store
            .get_operation(app_id, &holder.operation_id)
            .await
            .unwrap()
            .unwrap();
        let pause = super::super::operation_progress::DiagnosticReadPause::new();
        let restart = pause
            .clone()
            .scope(service.restart_app_controlled(app_id, control_request(request_id)));
        tokio::pin!(restart);
        tokio::select! {
            _ = pause.entered.wait() => {}
            result = restart.as_mut() => panic!("restart ended before diagnostic read: {result:?}"),
        }
        assert_not_admitted(&service, app_id, request_id).await;
        assert_no_restart_mutation(&runtime);
        if matches!(race, DiagnosticReleaseRace::RecoveryRequired) {
            holder
                .operation
                .fail(&AppOperationError::Backend(
                    "the original wake result remains uncertain".into(),
                ))
                .await
                .unwrap();
        } else {
            holder.finish().await;
        }
        let mut replacement = None;
        match race {
            DiagnosticReleaseRace::Released | DiagnosticReleaseRace::RecoveryRequired => {}
            DiagnosticReleaseRace::ReplacementStop | DiagnosticReleaseRace::ReplacementDelete => {
                let kind = if matches!(race, DiagnosticReleaseRace::ReplacementStop) {
                    UserAppOperationKind::Stop
                } else {
                    UserAppOperationKind::DeleteApplication
                };
                replacement = Some(
                    OwnedOperation::admit(
                        service.metadata.store.clone(),
                        UserAppAdmission {
                            runtime_policy_on_success: None,
                            command: None,
                            app_id: app_id.into(),
                            lifecycle_id: Some(original.lifecycle_id.clone()),
                            operation_id: uuid::Uuid::new_v4().to_string(),
                            request_id: Some("later-priority-operation".into()),
                            request_fingerprint: "d".repeat(64),
                            kind,
                            metadata: None,
                        },
                    )
                    .await
                    .unwrap(),
                );
            }
            DiagnosticReleaseRace::PriorityStop | DiagnosticReleaseRace::TerminalStopIntent => {
                let control = service
                    .metadata
                    .store
                    .admit_compute_control(&ComputeControlRequest {
                        app_id: app_id.into(),
                        lifecycle_id: original.lifecycle_id.clone(),
                        scope: UserAppOperationScope::Prod,
                        operation_id: uuid::Uuid::new_v4().to_string(),
                        request_id: "later-stop-intent".into(),
                        request_fingerprint: "f".repeat(64),
                        action: ComputeControlAction::Stop,
                        restart_image_roll: false,
                    })
                    .await
                    .unwrap();
                if matches!(race, DiagnosticReleaseRace::TerminalStopIntent) {
                    let identity = ComputeExecutorIdentity {
                        app_id: app_id.into(),
                        lifecycle_id: original.lifecycle_id.clone(),
                        scope: UserAppOperationScope::Prod,
                        operation_id: control.operation_id,
                        generation: control.generation,
                        executor_id: uuid::Uuid::new_v4().to_string(),
                    };
                    let claimed = service
                        .metadata
                        .store
                        .claim_compute_control(&identity, control.revision)
                        .await
                        .unwrap();
                    service
                        .metadata
                        .store
                        .advance_compute_control(&shared_types::ComputeControlProgress {
                            identity,
                            expected_revision: claimed.revision,
                            state: shared_types::ComputeControlState::Failed,
                            stage: shared_types::ComputeControlStage::DrainingPrevious,
                            checkpoint: serde_json::Value::Null,
                            error_code: Some("ERR_RUNTIME_UNAVAILABLE".into()),
                            error_message: Some("read-only stop preparation failed".into()),
                        })
                        .await
                        .unwrap();
                    let status = service
                        .metadata
                        .store
                        .read_compute_status(
                            app_id,
                            &original.lifecycle_id,
                            UserAppOperationScope::Prod,
                        )
                        .await
                        .unwrap();
                    assert!(status.desired_stopped);
                    assert!(status.operation.unwrap().state.is_terminal());
                }
            }
        }
        let (result, _) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(restart, pause.release.wait())
        })
        .await
        .expect("the original admission budget must bound the diagnostic race");
        if matches!(race, DiagnosticReleaseRace::Released) {
            result.unwrap();
            let accepted = service
                .metadata
                .store
                .get_operation_by_request(app_id, request_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(accepted.request_id.as_deref(), Some(request_id));
            assert_eq!(accepted.lifecycle_id, original.lifecycle_id);
            assert_eq!(accepted.kind, UserAppOperationKind::Restart);
            assert_eq!(accepted.state, UserAppOperationState::Succeeded);
            assert_ne!(accepted.operation_id, original.operation_id);
            assert_eq!(runtime.restart_calls.load(Ordering::SeqCst), 1);
            assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
        } else {
            assert!(result.is_err(), "later priority work must fence {race:?}");
            assert_not_admitted(&service, app_id, request_id).await;
            assert_no_restart_mutation(&runtime);
        }
        if replacement.is_some() {
            let preserved = service
                .metadata
                .store
                .get_operation_by_request(app_id, "later-priority-operation")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(preserved.state, UserAppOperationState::Running);
            assert_eq!(
                service
                    .get_lifecycle(app_id)
                    .await
                    .unwrap()
                    .active_operations
                    .slot(preserved.scope),
                Some(&preserved.operation_id),
                "the rejected waiting Restart must preserve the later operation's ownership"
            );
        }
    }
}

#[tokio::test]
async fn released_admission_observation_requires_exact_terminal_waitable_identity() {
    let directory = tempfile::tempdir().unwrap();
    let app_id = "releaseproof";
    let (service, runtime) = created_service(directory.path(), app_id, 2).await;
    let mut holder = traffic_holder(&service, app_id).await;
    holder.release_physical_guard().await;
    let original = service
        .metadata
        .store
        .get_operation(app_id, &holder.operation_id)
        .await
        .unwrap()
        .unwrap();
    let status = service
        .metadata
        .store
        .read_compute_status(app_id, &original.lifecycle_id, UserAppOperationScope::Prod)
        .await
        .unwrap();
    let captured = restart_wait::CapturedAdmissionBlocker {
        app_id: app_id.into(),
        lifecycle_id: original.lifecycle_id.clone(),
        blocker: original.blocker(),
        compute: restart_wait::AdmissionComputeFence::from(&status),
    };
    assert!(
        service
            .observe_released_admission_blocker(&captured)
            .await
            .unwrap()
            .is_none(),
        "a live durable holder is not released merely because its physical guard ended"
    );
    holder.finish().await;
    assert!(
        service
            .observe_released_admission_blocker(&captured)
            .await
            .unwrap()
            .is_some()
    );
    let mut unknown = captured.clone();
    unknown.blocker.operation_id = "missing-holder".into();
    assert!(
        service
            .observe_released_admission_blocker(&unknown)
            .await
            .unwrap()
            .is_none()
    );
    let mut foreign = captured.clone();
    foreign.lifecycle_id = "different-lifecycle".into();
    assert!(
        service
            .observe_released_admission_blocker(&foreign)
            .await
            .unwrap()
            .is_none()
    );
    let mut wrong_kind = captured;
    let mut recovery = wrong_kind.clone();
    recovery.blocker.state = UserAppOperationState::RecoveryRequired;
    assert!(
        service
            .observe_released_admission_blocker(&recovery)
            .await
            .unwrap()
            .is_none(),
        "an initially uncertain holder cannot gain queue permission from later terminal evidence"
    );
    wrong_kind.blocker.kind = UserAppOperationKind::Stop;
    assert!(
        service
            .observe_released_admission_blocker(&wrong_kind)
            .await
            .unwrap()
            .is_none()
    );
    assert_no_restart_mutation(&runtime);
}

#[tokio::test]
async fn waiting_restart_allows_original_business_restart_to_consume_old_stop_intent() {
    let directory = tempfile::tempdir().unwrap();
    let app_id = "restartresumemarker";
    let request_id = "waiting-restart-after-business-resume";
    let (service, runtime) = created_service(directory.path(), app_id, 2).await;
    let lifecycle = service.get_lifecycle(app_id).await.unwrap();
    let control = service
        .metadata
        .store
        .admit_compute_control(&ComputeControlRequest {
            app_id: app_id.into(),
            lifecycle_id: lifecycle.lifecycle_id.clone(),
            scope: UserAppOperationScope::Prod,
            operation_id: uuid::Uuid::new_v4().to_string(),
            request_id: "old-terminal-stop-intent".into(),
            request_fingerprint: "e".repeat(64),
            action: ComputeControlAction::Stop,
            restart_image_roll: false,
        })
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: app_id.into(),
        lifecycle_id: lifecycle.lifecycle_id.clone(),
        scope: UserAppOperationScope::Prod,
        operation_id: control.operation_id,
        generation: control.generation,
        executor_id: uuid::Uuid::new_v4().to_string(),
    };
    let claimed = service
        .metadata
        .store
        .claim_compute_control(&identity, control.revision)
        .await
        .unwrap();
    service
        .metadata
        .store
        .advance_compute_control(&shared_types::ComputeControlProgress {
            identity,
            expected_revision: claimed.revision,
            state: shared_types::ComputeControlState::Failed,
            stage: shared_types::ComputeControlStage::DrainingPrevious,
            checkpoint: serde_json::Value::Null,
            error_code: Some("ERR_RUNTIME_UNAVAILABLE".into()),
            error_message: Some("old stop preparation ended before physical mutation".into()),
        })
        .await
        .unwrap();
    let before = service
        .metadata
        .store
        .read_compute_status(app_id, &lifecycle.lifecycle_id, UserAppOperationScope::Prod)
        .await
        .unwrap();
    assert!(before.desired_stopped);
    assert!(before.operation.as_ref().unwrap().state.is_terminal());
    let mut holder = operation_holder(
        &service,
        app_id,
        UserAppOperationKind::Restart,
        Some(UserAppControlCommand::Restart),
        "business_restart_admitted",
    )
    .await;
    holder.release_physical_guard().await;
    let holder_id = holder.operation_id.clone();
    let pause = super::super::operation_progress::DiagnosticReadPause::new();
    let request = control_request(request_id);
    let restart = pause
        .clone()
        .scope(service.restart_app_controlled(app_id, request.clone()));
    tokio::pin!(restart);
    tokio::select! {
        _ = pause.entered.wait() => {}
        result = restart.as_mut() => panic!("restart ended before diagnostic read: {result:?}"),
    }
    assert_not_admitted(&service, app_id, request_id).await;
    assert_no_restart_mutation(&runtime);
    // The original business Restart legitimately does this after its admission.
    // It clears the old marker while keeping the same compute generation.
    service
        .metadata
        .store
        .check_compute_access(app_id, UserAppOperationScope::Prod, true)
        .await
        .unwrap();
    let resumed = service
        .metadata
        .store
        .read_compute_status(app_id, &lifecycle.lifecycle_id, UserAppOperationScope::Prod)
        .await
        .unwrap();
    assert_eq!(resumed.generation, before.generation);
    assert!(!resumed.desired_stopped);
    assert!(resumed.operation.is_none());
    holder.finish().await;
    let (result, _) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(restart, pause.release.wait())
    })
    .await
    .expect("same-generation business resume must keep the original wait budget");
    result.unwrap();
    let accepted = service
        .metadata
        .store
        .get_operation_by_request(app_id, request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(accepted.request_id.as_deref(), Some(request_id));
    assert_eq!(accepted.lifecycle_id, lifecycle.lifecycle_id);
    assert_ne!(accepted.operation_id, holder_id);
    assert_eq!(accepted.state, UserAppOperationState::Succeeded);
    assert_eq!(runtime.restart_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
    service
        .restart_app_controlled(app_id, request)
        .await
        .unwrap();
    assert_eq!(runtime.restart_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
}

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
