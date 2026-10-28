use super::super::ToastyUserAppStore;
use super::*;
use shared_types::*;

#[path = "storage_removal.rs"]
mod storage_removal;

fn pvc(name: &str, uid: &str) -> AppResourceIdentity {
    AppResourceIdentity {
        kind: AppResourceKind::PersistentVolumeClaim,
        name: name.into(),
        uid: uid.into(),
        resource_version: Some("captured-version".into()),
    }
}

async fn purge(
    store: &ToastyUserAppStore,
    found: &UserAppDiscoveredIdentity,
    id: String,
    prod: Vec<AppResourceIdentity>,
    dev: Vec<AppResourceIdentity>,
) -> UserAppOperationRecord {
    let UserAppAdmissionOutcome::Accepted(mut operation) = store
        .admit(&UserAppAdmission {
            app_id: found.app_id.clone(),
            lifecycle_id: Some(found.lifecycle_id.clone()),
            operation_id: id,
            request_id: None,
            request_fingerprint: "a".repeat(64),
            kind: UserAppOperationKind::PurgeResources,
            command: Some(UserAppControlCommand::DeleteResources {
                purge: true,
                expected_resource_version: None,
            }),
            metadata: None,
            runtime_policy_on_success: None,
        })
        .await
        .unwrap()
    else {
        panic!("fresh purge");
    };
    let progress =
        |operation: &UserAppOperationRecord, state, checkpoint| UserAppOperationProgress {
            app_id: operation.app_id.clone(),
            lifecycle_id: operation.lifecycle_id.clone(),
            operation_id: operation.operation_id.clone(),
            expected_revision: operation.revision,
            executor_id: "original-purge-executor".into(),
            state,
            step: "deleting-resources".into(),
            checkpoint,
            error_code: None,
            error_message: None,
        };
    operation = store
        .advance(&progress(
            &operation,
            UserAppOperationState::Running,
            serde_json::Value::Null,
        ))
        .await
        .unwrap();
    let context = UserAppExecutionContext {
        app_id: found.app_id.clone(),
        lifecycle_id: found.lifecycle_id.clone(),
        operation_id: operation.operation_id.clone(),
        executor_id: "original-purge-executor".into(),
        request_fingerprint: operation.request_fingerprint.clone(),
    };
    store
        .bind_operation_lease(
            &context,
            &UserAppOperationLeaseReceipt::Docker {
                service_type: ServiceType::Userapp,
                device: 1,
                inode: 2,
                token: operation.operation_id.clone(),
            },
        )
        .await
        .unwrap();
    let mut checkpoint = UserAppDeletionCheckpoint {
        schema_version: 1,
        stage: UserAppDeletionStage::Captured,
        context,
        kind: UserAppOperationKind::PurgeResources,
        production: AppDeletionSnapshot {
            app_id: found.app_id.clone(),
            operation_id: "captured-prod".into(),
            resources: prod,
            directories: None,
        },
        development: Some(UserappDevDeletionReceipt {
            runtime: BuilderDeletionSnapshot {
                app_id: found.app_id.clone(),
                operation_id: "captured-dev".into(),
                resources: dev,
                docker_bind_cleanup: false,
                resource_binding: None,
            },
            registry: None,
            lease: None,
            collaborators: Vec::new(),
            directories: None,
        }),
    };
    for stage in [
        UserAppDeletionStage::Captured,
        UserAppDeletionStage::ComputeRemoved,
        UserAppDeletionStage::ProductionStorageRemoved,
        UserAppDeletionStage::DevelopmentRemoved,
    ] {
        checkpoint.stage = stage;
        operation = store
            .advance(&progress(
                &operation,
                UserAppOperationState::Running,
                serde_json::to_value(&checkpoint).unwrap(),
            ))
            .await
            .unwrap();
    }
    store
        .advance(&progress(
            &operation,
            UserAppOperationState::Succeeded,
            serde_json::to_value(checkpoint).unwrap(),
        ))
        .await
        .unwrap()
}

async fn raw(store: &ToastyUserAppStore, app_id: &str) -> Vec<toasty_core::stmt::Value> {
    let app_id = app_id.to_owned();
    store.run(true, move |tx, backend| {
        let app_id = app_id.clone();
        Box::pin(async move {
            let mut rows = toasty::sql::query(super::super::repo::sql(backend, "SELECT witness_json FROM userapp_recovery_witnesses WHERE app_id=$1")).bind(&app_id).exec(tx).await.map_err(storage)?;
            rows.extend(toasty::sql::query(super::super::repo::sql(backend, "SELECT operation_id,checkpoint_json,revision,state FROM userapp_operations WHERE app_id=$1 ORDER BY operation_id")).bind(app_id).exec(tx).await.map_err(storage)?);
            Ok(rows)
        })
    }).await.unwrap()
}

async fn contract(store: &ToastyUserAppStore) {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let found = UserAppDiscoveredIdentity {
        app_id: format!("witness-{suffix}"),
        lifecycle_id: format!("life-{suffix}"),
        dev_uid: Some("old-builder".into()),
        prod_uid: Some("old-app".into()),
        dev_stopped: false,
        prod_stopped: false,
        dev_volumes: vec![pvc("dev-a", "old-dev-a"), pvc("dev-b", "old-dev-b")],
        prod_volumes: vec![pvc("prod-a", "old-prod-a"), pvc("prod-b", "old-prod-b")],
    };
    store.restore_discovered_identity(&found).await.unwrap();
    assert_eq!(
        store
            .get_recovery_storage_witness(&found.app_id, &found.lifecycle_id)
            .await
            .unwrap(),
        Some(found.clone())
    );
    let first = purge(
        store,
        &found,
        format!("a-{suffix}"),
        vec![found.prod_volumes[0].clone()],
        vec![found.dev_volumes[0].clone()],
    )
    .await;
    let remaining = store
        .get_recovery_storage_witness(&found.app_id, &found.lifecycle_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(remaining.prod_volumes, found.prod_volumes[1..]);
    assert_eq!(remaining.dev_volumes, found.dev_volumes[1..]);
    for n in 0..RETIREMENT_PAGE_SIZE + 1 {
        purge(
            store,
            &found,
            format!("m-{suffix}-{n:03}"),
            vec![pvc("prod-b", "different-new-uid")],
            vec![pvc("dev-b", "different-new-uid")],
        )
        .await;
    }
    let before = raw(store, &found.app_id).await;
    assert_eq!(
        store
            .get_recovery_storage_witness(&found.app_id, &found.lifecycle_id)
            .await
            .unwrap()
            .unwrap(),
        remaining,
        "same names with other UIDs cannot retire either scope"
    );
    assert_eq!(
        raw(store, &found.app_id).await,
        before,
        "projection never rewrites history"
    );
    purge(
        store,
        &found,
        format!("z-{suffix}"),
        vec![found.prod_volumes[1].clone()],
        vec![found.dev_volumes[1].clone()],
    )
    .await;
    let before = raw(store, &found.app_id).await;
    let remaining = store
        .get_recovery_storage_witness(&found.app_id, &found.lifecycle_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        remaining.prod_volumes.is_empty() && remaining.dev_volumes.is_empty(),
        "later page must be reached, not only latest/first receipt"
    );
    assert_eq!(raw(store, &found.app_id).await, before);
    assert_eq!(
        store
            .get_recovery_witness(&found.app_id, &found.lifecycle_id)
            .await
            .unwrap(),
        Some(found.clone())
    );

    // Identity, completion and unknown-write negatives use the exact real
    // succeeded receipt; mutating its clone grants no storage retirement.
    let mut candidates = Vec::new();
    let mut foreign = first.clone();
    foreign.state = UserAppOperationState::RecoveryRequired;
    candidates.push(foreign);
    let mut foreign = first.clone();
    foreign.app_id = "foreign-app".into();
    candidates.push(foreign);
    let mut foreign = first.clone();
    foreign.lifecycle_id = "foreign-lifecycle".into();
    candidates.push(foreign);
    let mut foreign = first.clone();
    foreign.checkpoint["context"]["executor_id"] = "foreign-executor".into();
    candidates.push(foreign);
    let mut foreign = first.clone();
    foreign.checkpoint["stage"] = "production_storage_removed".into();
    candidates.push(foreign);
    let mut foreign = first.clone();
    foreign.state = UserAppOperationState::Failed;
    candidates.push(foreign);
    for foreign in candidates {
        let mut projected = found.clone();
        assert!(subtract_confirmed_retirement(&mut projected, &foreign).is_err());
        assert_eq!(projected, found);
    }
    let mut later = found.clone();
    later.prod_volumes = vec![pvc("prod-a", "later-witness-new-uid")];
    later.dev_volumes.clear();
    subtract_confirmed_retirement(&mut later, &first).unwrap();
    assert_eq!(
        later.prod_volumes,
        vec![pvc("prod-a", "later-witness-new-uid")]
    );
    // An identical old UID remains retired even if a stale witness arrives later;
    // no timestamp or local wall clock is used as a new admission gate.
    let mut late_old = found.clone();
    subtract_confirmed_retirement(&mut late_old, &first).unwrap();
    assert_eq!(late_old.prod_volumes, found.prod_volumes[1..]);
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn turso_recovery_witness_purge_projection_preserves_history_and_exact_scope_uids() {
    let directory = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&directory.path().join("witness.db"))
        .await
        .unwrap();
    contract(&store).await;
    storage_removal::contract(&store).await;
    store.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
async fn postgres_recovery_witness_purge_projection_preserves_history_and_exact_scope_uids() {
    let Some(dsn) = crate::pg::test_support::test_dsn().await else {
        return;
    };
    let store = ToastyUserAppStore::connect(&crate::config::PostgresConfig {
        url: Some(dsn),
        ..Default::default()
    })
    .await
    .unwrap();
    contract(&store).await;
    storage_removal::contract(&store).await;
    store.shutdown().await.unwrap();
}
