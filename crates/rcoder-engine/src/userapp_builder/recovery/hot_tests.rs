//! Real lifecycle transactions through the original scanner. Runtime protocol
//! fixtures identify the original owner; they never pretend to deploy an app.
use super::*;
use crate::userapp_builder::creation::fence_settler_tests::{
    FenceRuntime, ReceiptVerdict, runtime_state_with_mode,
};
use container_runtime_api::{AppOperationLeaseObservation as Observation, ContainerRuntimeError};
use shared_types::*;
use std::{
    collections::HashMap,
    sync::{Arc, atomic::Ordering},
};

// discover polls tasks while awaiting later storage pages. Capture only this
// operation's real scanner warnings as well as tasks still queued on return;
// neither scheduling order may hide the recovery result from the assertions.
#[derive(Clone)]
struct ScanErrors {
    operation_id: String,
    messages: Arc<std::sync::Mutex<Vec<String>>>,
}
#[derive(Default)]
struct ErrorFields {
    operation_id: Option<String>,
    error: Option<String>,
}
impl tracing::field::Visit for ErrorFields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let value = format!("{value:?}");
        match field.name() {
            "operation_id" => self.operation_id = Some(value.trim_matches('"').to_owned()),
            "error" => self.error = Some(value),
            _ => {}
        }
    }
}
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ScanErrors {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() != module_path!().trim_end_matches("::hot_tests") {
            return;
        }
        let mut fields = ErrorFields::default();
        event.record(&mut fields);
        if fields.operation_id.as_deref() == Some(self.operation_id.as_str())
            && let Some(error) = fields.error
        {
            self.messages.lock().unwrap().push(error);
        }
    }
}

enum Backend {
    Local,
    #[cfg(feature = "rcoder-pg")]
    Postgres,
}

struct Fixture {
    state: Arc<AppState>,
    runtime: Arc<FenceRuntime>,
    record: UserAppOperationRecord,
    binding: UserAppOperationLeaseBinding,
    desired: HashMap<String, String>,
    original_env: AppEnvSnapshot,
    store: Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    _directory: tempfile::TempDir,
    _pg_schema: Option<String>,
}

fn progress(
    record: &UserAppOperationRecord,
    state: UserAppOperationState,
    step: &str,
    checkpoint: serde_json::Value,
) -> UserAppOperationProgress {
    UserAppOperationProgress {
        app_id: record.app_id.clone(),
        lifecycle_id: record.lifecycle_id.clone(),
        operation_id: record.operation_id.clone(),
        expected_revision: record.revision,
        executor_id: "original-hot-executor".into(),
        state,
        step: step.into(),
        checkpoint,
        error_code: (state == UserAppOperationState::RecoveryRequired)
            .then(|| "ERR_BACKEND_ERROR".into()),
        error_message: (state == UserAppOperationState::RecoveryRequired)
            .then(|| "original conditional env result was not confirmed".into()),
    }
}

impl Fixture {
    async fn new(
        backend: Backend,
        step: &str,
        state: UserAppOperationState,
        with_precondition: bool,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let (store, pg_schema) = match backend {
            Backend::Local => (
                rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                    &directory.path().join("hot-recovery.db"),
                )
                .await
                .unwrap(),
                None,
            ),
            #[cfg(feature = "rcoder-pg")]
            Backend::Postgres => {
                let dsn = std::env::var("RCODER_PG_TEST_DSN").expect(
                    "RCODER_PG_TEST_DSN is required for the explicit hot recovery PG contract",
                );
                assert!(!dsn.is_empty());
                // The original scanner reads every unfinished operation. An
                // app-ID prefix is not isolation: another fixture's runtime
                // cannot observe this owner's resources. Use a private schema,
                // matching the storage identity-constraint tests, and retain
                // it for diagnosis rather than filtering the real scan.
                let schema = format!("hot_recovery_{}", uuid::Uuid::new_v4().simple());
                let mut admin = toasty::Db::builder().connect(&dsn).await.unwrap();
                toasty::sql::statement(format!("CREATE SCHEMA {schema}"))
                    .exec(&mut admin)
                    .await
                    .unwrap();
                let separator = if dsn.contains('?') { '&' } else { '?' };
                let scoped = format!("{dsn}{separator}options=-csearch_path%3D{schema}");
                let store = rcoder_storage::userapp_lifecycle::TursoUserAppStore::connect(
                    &rcoder_storage::config::PostgresConfig {
                        url: Some(scoped),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
                eprintln!("Real PG hot recovery fixture schema retained: {schema}");
                (store, Some(schema))
            }
        };
        let store = Arc::new(store);
        let runtime = FenceRuntime::scenario(None, None);
        *runtime.hot_observation.lock().unwrap() = Some(Observation::ExpiredOriginalIdentity);
        runtime
            .compute_release_succeeds
            .store(true, Ordering::SeqCst);
        let app_id = format!("hot{}", uuid::Uuid::new_v4().simple());
        let app = store.ensure_identity(&app_id).await.unwrap();
        let operation_id = format!("op{}", uuid::Uuid::new_v4().simple());
        let pending = match store
            .admit(&UserAppAdmission {
                app_id: app_id.clone(),
                lifecycle_id: Some(app.lifecycle_id.clone()),
                operation_id: operation_id.clone(),
                request_id: Some(format!("req{operation_id}")),
                request_fingerprint: "ab".repeat(32),
                kind: UserAppOperationKind::HotDeploy,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            })
            .await
            .unwrap()
        {
            UserAppAdmissionOutcome::Accepted(operation) => operation,
            other => panic!("fresh fixture admission: {other:?}"),
        };
        let claimed = store
            .advance(&progress(
                &pending,
                UserAppOperationState::Running,
                "claimed",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        let context = UserAppExecutionContext {
            app_id,
            lifecycle_id: app.lifecycle_id,
            operation_id: operation_id.clone(),
            executor_id: "original-hot-executor".into(),
            request_fingerprint: "ab".repeat(32),
        };
        let binding = UserAppOperationLeaseBinding {
            context: context.clone(),
            receipt: UserAppOperationLeaseReceipt::Kubernetes {
                service_type: ServiceType::Userapp,
                namespace: "lease-test".into(),
                name: format!("rcoder-operation-prod-{}", context.app_id),
                uid: "original-lease-uid".into(),
                resource_version: "original-lease-rv".into(),
                token: "original-random-legacy-token".into(),
            },
        };
        store
            .bind_operation_lease(&context, &binding.receipt)
            .await
            .unwrap();
        let target = RuntimeConfigurationTarget {
            physical_uid: "original-pod-uid".into(),
            deployment_generation: "original-management-generation".into(),
        };
        *runtime.hot_target.lock().unwrap() = Some(target.clone());
        let original_env = AppEnvSnapshot {
            env: HashMap::from([("APP_RELEASE_ID".into(), "release-A".into())]),
            deployment_uid: Some("original-deployment-uid".into()),
            resource_name: Some("original-env".into()),
            resource_uid: Some("original-env-uid".into()),
            resource_version: Some("before-env-write".into()),
            ..Default::default()
        };
        let desired = HashMap::from([
            ("APP_RELEASE_ID".into(), "release-B".into()),
            (APP_DEPLOY_OPERATION_ID.into(), operation_id.clone()),
        ]);
        let mut checkpoint = serde_json::json!({"hot_execution": {
            "context": context, "target": target, "phase": "submit", "receipt_protocol": 1,
            "release_id": "release-B", "previous_evidence": {"retained": "original payload"}
        }});
        if with_precondition {
            checkpoint["hot_execution"]["converge_env"] = serde_json::to_value(&desired).unwrap();
            checkpoint["hot_execution"]["converge_precondition"] = serde_json::to_value(
                HotDeploymentEnvPrecondition::from_snapshot(&original_env).unwrap(),
            )
            .unwrap();
        }
        if with_precondition {
            checkpoint["hot_execution"]["converge_precondition"]["writer_diagnostic"] =
                serde_json::json!({"retained": "future optional diagnostic"});
        }
        let mut record = store
            .advance(&progress(
                &claimed,
                UserAppOperationState::Running,
                "hot_execution",
                checkpoint.clone(),
            ))
            .await
            .unwrap();
        if step == "hot_converging" {
            checkpoint["hot_execution"]["phase"] = "converging".into();
            checkpoint["hot_execution"]["converge_env"] = serde_json::to_value(&desired).unwrap();
            if with_precondition {
                checkpoint["hot_execution"]["converge_precondition"] = serde_json::to_value(
                    HotDeploymentEnvPrecondition::from_snapshot(&original_env).unwrap(),
                )
                .unwrap();
            }
            if with_precondition {
                checkpoint["hot_execution"]["converge_precondition"]["writer_diagnostic"] =
                    serde_json::json!({"retained": "future optional diagnostic"});
            }
            record = store
                .advance(&progress(
                    &record,
                    UserAppOperationState::Running,
                    step,
                    checkpoint,
                ))
                .await
                .unwrap();
        }
        if state == UserAppOperationState::RecoveryRequired {
            record = store
                .advance(&progress(&record, state, step, record.checkpoint.clone()))
                .await
                .unwrap();
        }
        assert_eq!(record.state, state);
        let state = runtime_state_with_mode(
            runtime.clone(),
            store.clone(),
            None,
            app_manager::config::AppAccessMode::Kubernetes,
        )
        .await;
        let fixture = Self {
            state,
            runtime,
            record,
            binding,
            desired,
            original_env,
            store,
            _directory: directory,
            _pg_schema: pg_schema,
        };
        fixture.owner("running", true, "original-management-generation");
        fixture
    }

    fn owner(&self, phase: &str, persisted: bool, generation: &str) {
        *self.runtime.hot_owner.lock().unwrap() = Some(serde_json::json!({"data": {
            "protocol_version": APP_CLI_UNIFIED_DEPLOY_PROTOCOL, "phase": phase,
            "operation": {"operation_id": self.record.operation_id,
                "deployment_generation_id": generation, "persisted": persisted,
                "request_release_id": "release-B", "artifact_release_id": "release-B",
                "phase": phase, "error": if phase == "failed" {Some("original build failed")} else {None},
                "recovery": null}
        }}));
    }

    fn env(&self, values: HashMap<String, String>, version: &str) {
        *self.runtime.hot_env.lock().unwrap() = Some(AppEnvSnapshot {
            env: values,
            resource_version: Some(version.into()),
            ..self.original_env.clone()
        });
    }

    async fn scan(&self) -> Vec<anyhow::Error> {
        use tracing::instrument::WithSubscriber as _;
        use tracing_subscriber::prelude::*;
        let captured = ScanErrors {
            operation_id: self.record.operation_id.clone(),
            messages: Arc::default(),
        };
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let mut errors = async {
            let mut tasks = RecoveryTasks::default();
            let mut cursor = None;
            discover(&self.state, &mut tasks, &mut cursor)
                .await
                .unwrap();
            let mut errors = Vec::new();
            while let Some((_, result)) = tasks.next().await {
                if let Err(error) = result {
                    errors.push(error);
                }
            }
            errors
        }
        .with_subscriber(subscriber)
        .await;
        errors.extend(
            captured
                .messages
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .map(anyhow::Error::msg),
        );
        errors
    }

    async fn current(&self) -> UserAppOperationRecord {
        self.store
            .get_operation(&self.record.app_id, &self.record.operation_id)
            .await
            .unwrap()
            .unwrap()
    }

    fn assert_no_replay(&self) {
        assert_eq!(
            self.runtime.hot_writes.load(Ordering::SeqCst),
            0,
            "recovery cannot replay env writes"
        );
        assert_eq!(
            self.runtime.compute_prepares.load(Ordering::SeqCst),
            0,
            "recovery cannot start another compute operation"
        );
    }

    async fn assert_released(&self) {
        assert!(
            self.store
                .get_application(&self.record.app_id)
                .await
                .unwrap()
                .unwrap()
                .active_operations
                .prod
                .is_none()
        );
        assert!(
            self.store
                .get_operation_lease(&self.record.app_id, &self.record.operation_id)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(self.runtime.compute_releases.load(Ordering::SeqCst), 1);
        self.assert_no_replay();
        let admission = self
            .store
            .admit(&UserAppAdmission {
                app_id: self.record.app_id.clone(),
                lifecycle_id: Some(self.record.lifecycle_id.clone()),
                operation_id: format!("next{}", uuid::Uuid::new_v4().simple()),
                request_id: None,
                request_fingerprint: "cd".repeat(32),
                kind: UserAppOperationKind::Restart,
                command: Some(UserAppControlCommand::Restart),
                metadata: None,
                runtime_policy_on_success: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(admission, UserAppAdmissionOutcome::Accepted(_)),
            "a fresh ordinary request must be admitted"
        );
    }
}

async fn expired_owner_contract(backend: Backend) {
    let fixture = Fixture::new(
        backend,
        "hot_execution",
        UserAppOperationState::RecoveryRequired,
        false,
    )
    .await;
    assert!(fixture.scan().await.is_empty());
    let terminal = fixture.current().await;
    assert_eq!(
        terminal.state,
        UserAppOperationState::Failed,
        "legacy submit-stage activation without confirmed saved config is partial failure, not full deployment success"
    );
    assert_eq!(
        terminal.checkpoint["hot_execution"],
        fixture.record.checkpoint["hot_execution"]
    );
    assert!(terminal.checkpoint.get("fence_released_evidence").is_some());
    assert_eq!(fixture.runtime.hot_reads.load(Ordering::SeqCst), 1);
    fixture.assert_released().await;
    fixture.store.shutdown().await.unwrap();
}

#[tokio::test]
async fn scanner_reads_original_hot_owner_after_same_identity_lease_expiry() {
    expired_owner_contract(Backend::Local).await;
}

#[cfg(feature = "rcoder-pg")]
#[tokio::test]
async fn pg_scanner_reads_original_hot_owner_after_same_identity_lease_expiry() {
    expired_owner_contract(Backend::Postgres).await;
}

async fn converging_contract(backend: Backend, state: UserAppOperationState) {
    let fixture = Fixture::new(backend, "hot_converging", state, true).await;
    fixture.env(fixture.desired.clone(), "committed-env-write");
    assert!(fixture.scan().await.is_empty());
    let terminal = fixture.current().await;
    assert_eq!(terminal.state, UserAppOperationState::Succeeded);
    assert_eq!(
        terminal.checkpoint["hot_execution"],
        fixture.record.checkpoint["hot_execution"]
    );
    assert_eq!(
        fixture.runtime.hot_reads.load(Ordering::SeqCst),
        0,
        "exact matched configuration is sufficient"
    );
    if state == UserAppOperationState::Running {
        assert!(
            fixture
                .store
                .check_business_execution(&fixture.binding.context)
                .await
                .is_err(),
            "late coordinator cannot write after snapshot reservation"
        );
    }
    fixture.assert_released().await;
    fixture.store.shutdown().await.unwrap();
}

#[tokio::test]
async fn scanner_recovers_hot_converging_after_configuration_response_loss() {
    converging_contract(Backend::Local, UserAppOperationState::RecoveryRequired).await;
}

#[tokio::test]
async fn scanner_reserves_crashed_running_hot_converging_before_finalizing() {
    converging_contract(Backend::Local, UserAppOperationState::Running).await;
}

#[cfg(feature = "rcoder-pg")]
#[tokio::test]
async fn pg_scanner_recovers_hot_converging_after_configuration_response_loss() {
    converging_contract(Backend::Postgres, UserAppOperationState::RecoveryRequired).await;
}

#[cfg(feature = "rcoder-pg")]
#[tokio::test]
async fn pg_scanner_reserves_crashed_running_hot_converging_before_finalizing() {
    converging_contract(Backend::Postgres, UserAppOperationState::Running).await;
}

async fn newer_config_contract(backend: Backend) {
    let fixture = Fixture::new(
        backend,
        "hot_converging",
        UserAppOperationState::RecoveryRequired,
        true,
    )
    .await;
    let newer = HashMap::from([
        ("APP_RELEASE_ID".into(), "release-C".into()),
        ("USER_VALUE".into(), "retain-me".into()),
    ]);
    fixture.env(newer.clone(), "newer-env-revision");
    assert!(fixture.scan().await.is_empty());
    let terminal = fixture.current().await;
    assert_eq!(
        terminal.state,
        UserAppOperationState::Failed,
        "activated owner plus conflicting saved configuration is partial failure, never success"
    );
    assert!(
        terminal
            .error_message
            .unwrap()
            .contains("original conditional env result was not confirmed")
    );
    assert_eq!(
        fixture
            .runtime
            .hot_env
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .env,
        newer
    );
    assert_eq!(
        terminal.checkpoint["hot_execution"],
        fixture.record.checkpoint["hot_execution"]
    );
    fixture.assert_released().await;
    fixture.store.shutdown().await.unwrap();
}

#[tokio::test]
async fn scanner_retires_old_conditional_write_and_preserves_newer_configuration() {
    newer_config_contract(Backend::Local).await;
}

#[cfg(feature = "rcoder-pg")]
#[tokio::test]
async fn pg_scanner_retires_old_conditional_write_and_preserves_newer_configuration() {
    newer_config_contract(Backend::Postgres).await;
}

#[tokio::test]
async fn unknown_old_env_write_preserves_protection_then_original_scan_can_retry() {
    for precondition in [false, true] {
        let fixture = Fixture::new(
            Backend::Local,
            "hot_converging",
            UserAppOperationState::RecoveryRequired,
            precondition,
        )
        .await;
        fixture.env(fixture.original_env.env.clone(), "before-env-write");
        let errors = fixture.scan().await;
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .to_string()
                .contains("conditional environment write is still unconfirmed")
        );
        assert_eq!(
            fixture.runtime.hot_reads.load(Ordering::SeqCst),
            1,
            "historical missing fields must not prevent reading the original owner"
        );
        assert_eq!(fixture.current().await, fixture.record);
        assert_eq!(fixture.runtime.compute_releases.load(Ordering::SeqCst), 0);
        fixture.assert_no_replay();
        fixture.env(fixture.desired.clone(), "late-original-env-commit");
        assert!(fixture.scan().await.is_empty());
        assert_eq!(
            fixture.current().await.state,
            UserAppOperationState::Succeeded
        );
        fixture.assert_released().await;
        fixture.store.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn running_hot_with_live_or_unconfirmed_lease_is_never_taken_over() {
    for observation in [Observation::Held, Observation::Unconfirmed] {
        let fixture = Fixture::new(
            Backend::Local,
            "hot_execution",
            UserAppOperationState::Running,
            false,
        )
        .await;
        *fixture.runtime.hot_observation.lock().unwrap() = Some(observation);
        assert!(fixture.scan().await.is_empty());
        assert_eq!(fixture.current().await, fixture.record);
        assert_eq!(fixture.runtime.hot_reads.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.runtime.compute_releases.load(Ordering::SeqCst), 0);
        fixture.assert_no_replay();
        fixture.store.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn new_holder_during_owner_read_and_foreign_owner_evidence_keep_original_slot() {
    for variant in [
        "holder",
        "generation",
        "operation",
        "unpersisted",
        "deploying",
        "failed",
    ] {
        let fixture = Fixture::new(
            Backend::Local,
            "hot_execution",
            UserAppOperationState::RecoveryRequired,
            false,
        )
        .await;
        match variant {
            "holder" => fixture
                .runtime
                .hot_observation_replies
                .lock()
                .unwrap()
                .extend([
                    Ok(Observation::ExpiredOriginalIdentity),
                    Err(ContainerRuntimeError::Conflict(
                        "Original operation lease identity changed".into(),
                    )),
                ]),
            "generation" => fixture.owner("running", true, "successor-generation"),
            "operation" => {
                fixture.runtime.hot_owner.lock().unwrap().as_mut().unwrap()["data"]["operation"]["operation_id"] =
                    "successor-operation".into()
            }
            "unpersisted" => fixture.owner("running", false, "original-management-generation"),
            "deploying" => fixture.owner("deploying", false, "original-management-generation"),
            "failed" => fixture.owner("failed", true, "original-management-generation"),
            _ => unreachable!(),
        }
        let errors = fixture.scan().await;
        if variant == "failed" {
            assert!(errors.is_empty());
            let terminal = fixture.current().await;
            assert_eq!(terminal.state, UserAppOperationState::Failed);
            assert_eq!(
                terminal.error_message.as_deref(),
                Some("original build failed")
            );
            fixture.assert_released().await;
        } else {
            if variant != "deploying" {
                assert_eq!(errors.len(), 1, "{variant}");
            }
            assert_eq!(fixture.current().await, fixture.record, "{variant}");
            assert_eq!(fixture.runtime.compute_releases.load(Ordering::SeqCst), 0);
            fixture.assert_no_replay();
        }
        fixture.store.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn hot_reservation_rejects_late_stage_snapshot_and_different_binding() {
    let fixture = Fixture::new(
        Backend::Local,
        "hot_execution",
        UserAppOperationState::Running,
        false,
    )
    .await;
    let mut checkpoint = fixture.record.checkpoint.clone();
    checkpoint["hot_execution"]["phase"] = "converging".into();
    checkpoint["hot_execution"]["converge_env"] = serde_json::to_value(&fixture.desired).unwrap();
    let advanced = fixture
        .store
        .advance(&progress(
            &fixture.record,
            UserAppOperationState::Running,
            "hot_converging",
            checkpoint,
        ))
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .store
            .reserve_interrupted_hot(&fixture.record, &fixture.binding)
            .await,
        Err(UserAppStoreError::VersionConflict)
    ));
    let mut wrong = fixture.binding.clone();
    if let UserAppOperationLeaseReceipt::Kubernetes { uid, .. } = &mut wrong.receipt {
        *uid = "successor-lease-uid".into();
    }
    assert!(matches!(
        fixture
            .store
            .reserve_interrupted_hot(&advanced, &wrong)
            .await,
        Err(UserAppStoreError::VersionConflict)
    ));
    assert_eq!(fixture.current().await, advanced);
    assert!(
        fixture
            .store
            .check_business_execution(&fixture.binding.context)
            .await
            .is_ok(),
        "a stale observer cannot revoke the advanced original coordinator"
    );
    fixture.assert_no_replay();
    fixture.store.shutdown().await.unwrap();
}

#[tokio::test]
async fn submit_stage_owner_success_requires_real_saved_configuration_or_releases_partial_failure()
{
    for state in [
        UserAppOperationState::Running,
        UserAppOperationState::RecoveryRequired,
    ] {
        for matched in [false, true] {
            let fixture = Fixture::new(Backend::Local, "hot_execution", state, true).await;
            fixture.env(
                if matched {
                    fixture.desired.clone()
                } else {
                    fixture.original_env.env.clone()
                },
                "current-env-revision",
            );
            assert!(fixture.scan().await.is_empty());
            let terminal = fixture.current().await;
            assert_eq!(
                terminal.state,
                if matched {
                    UserAppOperationState::Succeeded
                } else {
                    UserAppOperationState::Failed
                }
            );
            assert!(
                fixture
                    .store
                    .check_business_execution(&fixture.binding.context)
                    .await
                    .is_err()
            );
            fixture.assert_released().await;
            fixture.store.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn concurrent_priority_stop_keeps_hot_reservation_from_revoking_its_target() {
    let fixture = Fixture::new(
        Backend::Local,
        "hot_execution",
        UserAppOperationState::Running,
        false,
    )
    .await;
    let request = ComputeControlRequest {
        app_id: fixture.record.app_id.clone(),
        lifecycle_id: fixture.record.lifecycle_id.clone(),
        scope: UserAppOperationScope::Prod,
        action: ComputeControlAction::Stop,
        operation_id: format!("stop{}", uuid::Uuid::new_v4().simple()),
        request_id: "priority-stop-request".into(),
        request_fingerprint: "ef".repeat(32),
        restart_image_roll: false,
        builder_startup_mode: Default::default(),
    };
    let stop = fixture.store.admit_compute_control(&request).await.unwrap();
    assert!(
        stop.interrupted_operations
            .contains(&fixture.record.operation_id)
    );
    assert!(
        fixture
            .store
            .reserve_interrupted_hot(&fixture.record, &fixture.binding)
            .await
            .is_err()
    );
    assert_eq!(fixture.current().await, fixture.record);
    assert_eq!(fixture.runtime.compute_releases.load(Ordering::SeqCst), 0);
    fixture.assert_no_replay();
    fixture.store.shutdown().await.unwrap();
}

#[tokio::test]
async fn configuration_io_failure_keeps_original_record_and_retry_recovers_without_writes() {
    let fixture = Fixture::new(
        Backend::Local,
        "hot_converging",
        UserAppOperationState::RecoveryRequired,
        true,
    )
    .await;
    assert_eq!(fixture.scan().await.len(), 1);
    assert_eq!(fixture.current().await, fixture.record);
    assert_eq!(fixture.runtime.compute_releases.load(Ordering::SeqCst), 0);
    fixture.env(fixture.desired.clone(), "completed-env-revision");
    assert!(fixture.scan().await.is_empty());
    assert_eq!(
        fixture.current().await.state,
        UserAppOperationState::Succeeded
    );
    fixture.assert_released().await;
    fixture.store.shutdown().await.unwrap();
}

#[tokio::test]
async fn retained_pre_lease_receipt_remains_readable_without_authorizing_running_takeover() {
    for state in [
        UserAppOperationState::RecoveryRequired,
        UserAppOperationState::Running,
    ] {
        let fixture = Fixture::new(Backend::Local, "hot_execution", state, false).await;
        *fixture.runtime.hot_observation.lock().unwrap() = Some(Observation::Unconfirmed);
        *fixture.runtime.receipt_verdict.lock().unwrap() = ReceiptVerdict::Held;
        assert!(fixture.scan().await.is_empty());
        if state == UserAppOperationState::RecoveryRequired {
            assert_eq!(fixture.current().await.state, UserAppOperationState::Failed);
            assert_eq!(fixture.runtime.hot_reads.load(Ordering::SeqCst), 1);
            fixture.assert_released().await;
        } else {
            assert_eq!(fixture.current().await, fixture.record);
            assert_eq!(fixture.runtime.hot_reads.load(Ordering::SeqCst), 0);
            assert_eq!(fixture.runtime.compute_releases.load(Ordering::SeqCst), 0);
        }
        fixture.assert_no_replay();
        fixture.store.shutdown().await.unwrap();
    }
}

async fn incomplete_historical_environment_target_contract(variant: &str) {
    let mut fixture = Fixture::new(
        Backend::Local,
        "hot_converging",
        UserAppOperationState::Running,
        true,
    )
    .await;
    let mut checkpoint = fixture.record.checkpoint.clone();
    match variant {
        "missing" => {
            checkpoint["hot_execution"]
                .as_object_mut()
                .unwrap()
                .remove("converge_env");
        }
        "malformed" => {
            checkpoint["hot_execution"]["converge_env"] =
                serde_json::json!(["historical non-map value"])
        }
        _ => unreachable!(),
    }
    fixture.record = fixture
        .store
        .advance(&progress(
            &fixture.record,
            UserAppOperationState::RecoveryRequired,
            "hot_converging",
            checkpoint,
        ))
        .await
        .unwrap();
    fixture.env(
        fixture.original_env.env.clone(),
        "newer-config-retires-original-patch",
    );
    let errors = fixture.scan().await;
    assert!(
        errors.is_empty(),
        "known native write retirement must not be blocked by {variant} expected env: {errors:?}"
    );
    let terminal = fixture.current().await;
    assert_eq!(
        terminal.state,
        UserAppOperationState::Failed,
        "missing expected env can never authorize deployment success"
    );
    assert_eq!(
        terminal.checkpoint["hot_execution"], fixture.record.checkpoint["hot_execution"],
        "original historical fields and optional diagnostics must be retained"
    );
    assert!(
        terminal
            .error_message
            .unwrap()
            .contains("conditional environment write was retired")
    );
    assert_eq!(
        fixture.runtime.hot_reads.load(Ordering::SeqCst),
        1,
        "original owner must still be observed"
    );
    fixture.assert_released().await;
    fixture.store.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_historical_expected_env_does_not_block_proven_write_retirement() {
    incomplete_historical_environment_target_contract("missing").await;
}

#[tokio::test]
async fn malformed_historical_expected_env_does_not_block_proven_write_retirement() {
    incomplete_historical_environment_target_contract("malformed").await;
}

async fn submit_stage_historical_precondition_contract(value: serde_json::Value) {
    for state in [
        UserAppOperationState::Running,
        UserAppOperationState::RecoveryRequired,
    ] {
        let mut fixture = Fixture::new(
            Backend::Local,
            "hot_execution",
            UserAppOperationState::Running,
            true,
        )
        .await;
        let mut checkpoint = fixture.record.checkpoint.clone();
        checkpoint["hot_execution"]["converge_precondition"] = value.clone();
        fixture.record = fixture
            .store
            .advance(&progress(
                &fixture.record,
                state,
                "hot_execution",
                checkpoint,
            ))
            .await
            .unwrap();
        fixture.env(fixture.desired.clone(), "actual-matching-configuration");
        let errors = fixture.scan().await;
        assert!(
            errors.is_empty(),
            "invalid optional historical proof: {errors:?}"
        );
        let terminal = fixture.current().await;
        assert_eq!(
            terminal.state,
            UserAppOperationState::Succeeded,
            "real environment and exact owner success must not be overridden by an invalid optional historical precondition: {value}"
        );
        assert_eq!(
            terminal.checkpoint["hot_execution"], fixture.record.checkpoint["hot_execution"],
            "retain original historical bytes and diagnostics"
        );
        assert_eq!(fixture.runtime.hot_reads.load(Ordering::SeqCst), 1);
        fixture.assert_released().await;
        fixture.store.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn submit_stage_empty_historical_precondition_does_not_override_matching_environment() {
    for field in [
        "deployment_uid",
        "resource_name",
        "resource_uid",
        "resource_version",
    ] {
        let mut value = serde_json::json!({
            "deployment_uid": "original-deployment-uid", "resource_name": "original-env",
            "resource_uid": "original-env-uid", "resource_version": "before-env-write",
            "future_diagnostic": {"retained": true}
        });
        value[field] = serde_json::json!("");
        submit_stage_historical_precondition_contract(value).await;
    }
}

#[tokio::test]
async fn submit_stage_malformed_historical_precondition_does_not_override_matching_environment() {
    for value in [
        serde_json::Value::Null,
        serde_json::json!(["old non-object record"]),
        serde_json::json!({"resource_uid": 17}),
    ] {
        submit_stage_historical_precondition_contract(value).await;
    }
}

#[tokio::test]
async fn invalid_historical_precondition_does_not_prove_an_unconfirmed_convergence_write() {
    for value in [
        serde_json::json!({
            "deployment_uid": "original-deployment-uid", "resource_name": "original-env",
            "resource_uid": "", "resource_version": "before-env-write"
        }),
        serde_json::json!(["old non-object record"]),
    ] {
        let mut fixture = Fixture::new(
            Backend::Local,
            "hot_converging",
            UserAppOperationState::Running,
            true,
        )
        .await;
        let mut checkpoint = fixture.record.checkpoint.clone();
        checkpoint["hot_execution"]["converge_precondition"] = value;
        fixture.record = fixture
            .store
            .advance(&progress(
                &fixture.record,
                UserAppOperationState::RecoveryRequired,
                "hot_converging",
                checkpoint,
            ))
            .await
            .unwrap();
        fixture.env(
            fixture.original_env.env.clone(),
            "unconfirmed-newer-revision",
        );
        assert!(
            !fixture.scan().await.is_empty(),
            "invalid optional evidence cannot retire a possibly submitted env write"
        );
        assert_eq!(fixture.current().await, fixture.record);
        assert_eq!(fixture.runtime.compute_releases.load(Ordering::SeqCst), 0);
        fixture.assert_no_replay();
        fixture.store.shutdown().await.unwrap();
    }
}
