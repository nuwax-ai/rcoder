//! Kubernetes Restart stop-boundary confirmation contracts. Evidence is
//! produced engine-side from read-only runtime reconciliation; these storage
//! contracts never fabricate runtime facts.
use super::ToastyUserAppStore;
use shared_types::*;

fn request(app: &UserAppLifecycleRecord) -> ComputeControlRequest {
    let id = uuid::Uuid::new_v4().simple().to_string();
    ComputeControlRequest {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        scope: UserAppOperationScope::Dev,
        operation_id: id.clone(),
        request_id: id,
        request_fingerprint: "a".repeat(64),
        action: ComputeControlAction::Restart,
        restart_image_roll: false,
        builder_startup_mode: BuilderStartupMode::ManagementOnly,
    }
}

fn kubernetes_lease(executor_id: &str) -> UserAppOperationLeaseReceipt {
    UserAppOperationLeaseReceipt::Kubernetes {
        service_type: ServiceType::UserappBuilder,
        namespace: "rcoder-builder".into(),
        name: "rcoder-app-builder-stopfixture".into(),
        uid: "c".repeat(36),
        resource_version: "12".into(),
        token: executor_id.into(),
    }
}

/// A Dev Restart claimed by a dead executor and stuck at the Kubernetes
/// Stopping boundary with a captured StatefulSet target.
async fn captured_stopping(
    store: &ToastyUserAppStore,
    state: ComputeControlState,
    lease: UserAppOperationLeaseReceipt,
) -> ComputeControlRecord {
    let app = store
        .ensure_identity(&format!("stopsts{}", uuid::Uuid::new_v4().simple()))
        .await
        .unwrap();
    let pending = store.admit_compute_control(&request(&app)).await.unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: pending.app_id.clone(),
        lifecycle_id: pending.lifecycle_id.clone(),
        scope: pending.scope,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: "originalexecutor".into(),
    };
    store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let claimed = store.bind_compute_lease(&identity, &lease).await.unwrap();
    let target = BuilderControlTarget {
        context: UserAppExecutionContext {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            operation_id: pending.operation_id.clone(),
            executor_id: identity.executor_id.clone(),
            request_fingerprint: pending.request_fingerprint.clone(),
        },
        workload: Some(AppResourceIdentity {
            kind: AppResourceKind::StatefulSet,
            name: format!("rcoder-app-builder-{}", app.app_id),
            uid: "d".repeat(36),
            resource_version: Some("42".into()),
        }),
        pod: None,
        resource_binding: None,
        restart_image: Some("builder@sha256:original".into()),
        restart_runtime_workspace: None,
        builder_startup_mode: BuilderStartupMode::ManagementOnly,
    };
    let mut checkpoint = serde_json::to_value(&target).unwrap();
    checkpoint["builder_volumes"] = serde_json::to_value(vec![AppResourceIdentity {
        kind: AppResourceKind::PersistentVolumeClaim,
        name: "builder-workspace".into(),
        uid: "builder-workspace".into(),
        resource_version: None,
    }])
    .unwrap();
    let stopping = store
        .advance_compute_control(&ComputeControlProgress {
            identity: identity.clone(),
            expected_revision: claimed.revision,
            state: ComputeControlState::Running,
            stage: ComputeControlStage::Stopping,
            checkpoint,
            error_code: None,
            error_message: None,
        })
        .await
        .unwrap();
    match state {
        ComputeControlState::Running => stopping,
        ComputeControlState::RecoveryRequired => store
            .advance_compute_control(&ComputeControlProgress {
                identity,
                expected_revision: stopping.revision,
                state,
                stage: ComputeControlStage::Stopping,
                checkpoint: stopping.checkpoint,
                error_code: Some("ERR_STOP_REPLY_TIMEOUT".into()),
                error_message: Some("original scale-down response timed out".into()),
            })
            .await
            .unwrap(),
        ComputeControlState::Pending
        | ComputeControlState::Succeeded
        | ComputeControlState::Failed
        | ComputeControlState::Superseded => panic!("fixture state"),
    }
}

async fn contract(store: &ToastyUserAppStore) {
    // Positive: Running at stopping with receipt evidence advances exactly to
    // the durable stopped boundary, preserving target/lease/executor.
    let snapshot = captured_stopping(
        store,
        ComputeControlState::Running,
        kubernetes_lease("originalexecutor"),
    )
    .await;
    let confirmed = store
        .confirm_kubernetes_restart_stopped(
            &snapshot,
            KubernetesRestartStopEvidence::StopReceiptAcknowledged,
        )
        .await
        .unwrap();
    assert_eq!(confirmed.stage, "stopped");
    assert_eq!(confirmed.state, ComputeControlState::Running);
    assert_eq!(confirmed.revision, snapshot.revision + 1);
    assert_eq!(confirmed.lease, snapshot.lease);
    assert_eq!(confirmed.executor_id, snapshot.executor_id);
    let evidence: KubernetesRestartStopConfirmation = serde_json::from_value(
        confirmed
            .checkpoint
            .get(KUBERNETES_RESTART_STOP_CONFIRMATION_KEY)
            .cloned()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        evidence.evidence,
        KubernetesRestartStopEvidence::StopReceiptAcknowledged
    );
    assert_eq!(evidence.original_state, ComputeControlState::Running);
    // The original target stays readable for the start continuation.
    let target = BuilderControlTarget::from_observation_checkpoint(&confirmed.checkpoint).unwrap();
    assert_eq!(
        target,
        BuilderControlTarget::from_observation_checkpoint(&snapshot.checkpoint).unwrap()
    );

    // Positive: RecoveryRequired with observed-stop evidence also confirms and
    // preserves the original diagnostics inside the evidence record.
    let recovery = captured_stopping(
        store,
        ComputeControlState::RecoveryRequired,
        kubernetes_lease("originalexecutor"),
    )
    .await;
    let confirmed = store
        .confirm_kubernetes_restart_stopped(
            &recovery,
            KubernetesRestartStopEvidence::ObservedStopped,
        )
        .await
        .unwrap();
    assert_eq!(confirmed.stage, "stopped");
    let evidence: KubernetesRestartStopConfirmation = serde_json::from_value(
        confirmed
            .checkpoint
            .get(KUBERNETES_RESTART_STOP_CONFIRMATION_KEY)
            .cloned()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        evidence.original_state,
        ComputeControlState::RecoveryRequired
    );
    assert_eq!(
        evidence.original_error_code.as_deref(),
        Some("ERR_STOP_REPLY_TIMEOUT")
    );

    // Stale authority: replaying the first snapshot after its own confirmation
    // must not move the record again.
    assert!(matches!(
        store
            .confirm_kubernetes_restart_stopped(
                &snapshot,
                KubernetesRestartStopEvidence::StopReceiptAcknowledged
            )
            .await,
        Err(UserAppStoreError::VersionConflict)
    ));

    // A Docker lease never confirms through the Kubernetes boundary.
    let docker = captured_stopping(
        store,
        ComputeControlState::Running,
        UserAppOperationLeaseReceipt::Docker {
            service_type: ServiceType::UserappBuilder,
            device: 1,
            inode: 42,
            token: "originalexecutor".into(),
        },
    )
    .await;
    // Give the Docker fixture a Kubernetes-shaped target: the lease alone must
    // still reject it so the two boundary domains never overlap.
    let mut docker_target = docker.clone();
    docker_target.checkpoint = snapshot.checkpoint.clone();
    assert!(matches!(
        store
            .confirm_kubernetes_restart_stopped(
                &docker_target,
                KubernetesRestartStopEvidence::ObservedStopped
            )
            .await,
        Err(UserAppStoreError::InvalidOperation(_))
    ));
    // Conversely the Kubernetes lease with the Docker Container target is the
    // Docker domain and is rejected here.
    let mut sts_with_container_target = snapshot.clone();
    sts_with_container_target.lease = docker.lease.clone();
    assert!(matches!(
        store
            .confirm_kubernetes_restart_stopped(
                &sts_with_container_target,
                KubernetesRestartStopEvidence::ObservedStopped
            )
            .await,
        Err(UserAppStoreError::InvalidOperation(_))
    ));

    // Already-stopped records are not confirmable again: no unknown Create may
    // hide behind a re-confirmation.
    assert!(matches!(
        store
            .confirm_kubernetes_restart_stopped(
                &confirmed,
                KubernetesRestartStopEvidence::ObservedStopped
            )
            .await,
        Err(UserAppStoreError::InvalidOperation(_))
    ));

    // The confirm→continue chain: the start continuation must accept the
    // confirmed checkpoint through the strict full-target parse and preserve
    // the confirmation evidence (with its original diagnostics) in the
    // rebuilt starting checkpoint.
    let target = BuilderControlTarget::from_observation_checkpoint(&confirmed.checkpoint).unwrap();
    let resumed = store
        .resume_builder_restart_start(&confirmed, &target)
        .await
        .unwrap();
    assert_eq!(resumed.stage, "starting");
    assert_eq!(resumed.state, ComputeControlState::Running);
    assert_eq!(resumed.revision, confirmed.revision + 1);
    let evidence: KubernetesRestartStopConfirmation = serde_json::from_value(
        resumed
            .checkpoint
            .get(KUBERNETES_RESTART_STOP_CONFIRMATION_KEY)
            .cloned()
            .expect("confirmation evidence must survive the continuation CAS"),
    )
    .unwrap();
    assert_eq!(
        evidence.original_state,
        ComputeControlState::RecoveryRequired
    );
    assert_eq!(
        evidence.original_error_code.as_deref(),
        Some("ERR_STOP_REPLY_TIMEOUT")
    );
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn turso_kubernetes_restart_stop_confirmation_preserves_intent_and_rejects_stale_authority() {
    let directory = tempfile::tempdir().unwrap();
    let store =
        ToastyUserAppStore::open_exclusive(&directory.path().join("sts-stop-confirmation.db"))
            .await
            .unwrap();
    contract(&store).await;
    store.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicitly disposable PostgreSQL DSN; draft has not been executed"]
async fn pg_kubernetes_restart_stop_confirmation_preserves_intent_and_rejects_stale_authority() {
    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG DSN required")),
        max_connections: Some(1),
        min_connections: Some(1),
        statement_timeout_secs: Some(5),
        ..Default::default()
    };
    let store = ToastyUserAppStore::connect(&config).await.unwrap();
    contract(&store).await;
    store.shutdown().await.unwrap();
}
