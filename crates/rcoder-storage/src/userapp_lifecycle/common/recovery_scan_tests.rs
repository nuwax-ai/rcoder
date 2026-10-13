//! Real shared-store pagination keeps malformed history isolated and unchanged.
use super::{ToastyUserAppStore, repo, storage};
use crate::db::models;
use shared_types::*;

async fn seed(
    store: &ToastyUserAppStore,
    operation_id: String,
    terminal: bool,
) -> (
    UserAppOperationRecord,
    UserAppExecutionContext,
    Option<UserAppOperationLeaseBinding>,
) {
    let app = store
        .ensure_identity(&format!("scan{}", uuid::Uuid::new_v4().simple()))
        .await
        .unwrap();
    let request = UserAppAdmission {
        app_id: app.app_id.clone(),
        lifecycle_id: Some(app.lifecycle_id.clone()),
        operation_id,
        request_id: None,
        request_fingerprint: "a".repeat(64),
        kind: UserAppOperationKind::Start,
        command: None,
        metadata: None,
        runtime_policy_on_success: None,
    };
    let UserAppAdmissionOutcome::Accepted(mut operation) = store.admit(&request).await.unwrap()
    else {
        panic!("fresh isolated operation must be admitted");
    };
    let context = UserAppExecutionContext {
        app_id: operation.app_id.clone(),
        lifecycle_id: operation.lifecycle_id.clone(),
        operation_id: operation.operation_id.clone(),
        executor_id: "scan-fixture-executor".into(),
        request_fingerprint: operation.request_fingerprint.clone(),
    };
    if !terminal {
        return (operation, context, None);
    }
    operation = store
        .advance(&progress(&operation, UserAppOperationState::Running))
        .await
        .unwrap();
    let receipt = UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::Userapp,
        device: 1,
        inode: 42,
        token: operation.operation_id.clone(),
    };
    store
        .bind_operation_lease(&context, &receipt)
        .await
        .unwrap();
    let binding = store
        .get_operation_lease(&operation.app_id, &operation.operation_id)
        .await
        .unwrap()
        .unwrap();
    operation = store
        .advance(&progress(&operation, UserAppOperationState::Failed))
        .await
        .unwrap();
    (operation, context, Some(binding))
}

fn progress(
    operation: &UserAppOperationRecord,
    state: UserAppOperationState,
) -> UserAppOperationProgress {
    UserAppOperationProgress {
        app_id: operation.app_id.clone(),
        lifecycle_id: operation.lifecycle_id.clone(),
        operation_id: operation.operation_id.clone(),
        expected_revision: operation.revision,
        executor_id: "scan-fixture-executor".into(),
        state,
        step: "fixture".into(),
        checkpoint: serde_json::Value::Null,
        error_code: None,
        error_message: None,
    }
}

async fn corrupt_operation(
    store: &ToastyUserAppStore,
    operation: &UserAppOperationRecord,
    field: &'static str,
) {
    let id = operation.operation_id.clone();
    store.run(false, move |tx,backend| Box::pin(async move {
        let sql=match field {
            // Current catalogs constrain payload_version=1. A historical
            // typed JSON schema can still be stored without weakening DDL.
            "command-schema" => "UPDATE userapp_operations SET command_json='{\"type\":\"historical_command_v999\"}' WHERE operation_id=$1",
            "checkpoint" => "UPDATE userapp_operations SET checkpoint_json='{malformed' WHERE operation_id=$1",
            "fingerprint" => "UPDATE userapp_operations SET request_fingerprint='gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg' WHERE operation_id=$1",
            _ => panic!("unknown fixture corruption"),
        };
        toasty::sql::statement(repo::sql(backend,sql)).bind(&id).exec(tx).await.map_err(storage)?;
        Ok(())
    })).await.unwrap();
}

async fn corrupt_lease(
    store: &ToastyUserAppStore,
    operation: &UserAppOperationRecord,
    field: &'static str,
) {
    let id = operation.operation_id.clone();
    store.run(false, move |tx,backend| Box::pin(async move {
        let sql=match field {
            // The version column is constrained too; an unknown receipt
            // version in its JSON is a real decoder error in both backends.
            "receipt-schema" => "UPDATE userapp_operation_leases SET receipt_json='{\"runtime\":\"historical_runtime_v999\",\"version\":999}' WHERE operation_id=$1",
            "receipt" => "UPDATE userapp_operation_leases SET receipt_json='{malformed' WHERE operation_id=$1",
            "fingerprint" => "UPDATE userapp_operation_leases SET request_fingerprint='gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg' WHERE operation_id=$1",
            _ => panic!("unknown fixture corruption"),
        };
        toasty::sql::statement(repo::sql(backend,sql)).bind(&id).exec(tx).await.map_err(storage)?;
        Ok(())
    })).await.unwrap();
}

/// Serialize every raw persisted column, including the original JSON strings.
/// Recovery reads must not repair/drop data or make a malformed row writable.
async fn operation_bytes(store: &ToastyUserAppStore, operation_id: &str) -> Vec<u8> {
    let id = operation_id.to_owned();
    store.run(true, move |tx,_| Box::pin(async move {
        let row=models::Operation::get_by_operation_id(tx,&id).await.map_err(storage)?;
        serde_json::to_vec(&serde_json::json!({
            "operation_id":row.operation_id,"app_id":row.app_id,"lifecycle_id":row.lifecycle_id,
            "kind":row.kind,"scope":row.scope,"state":row.state,"revision":row.revision,
            "origin_request_id":row.origin_request_id,"request_fingerprint":row.request_fingerprint,
            "executor_id":row.executor_id,"step":row.step,"error_code":row.error_code,"error_message":row.error_message,
            "payload_version":row.payload_version,"command_json":row.command_json,
            "admitted_metadata_json":row.admitted_metadata_json,"runtime_policy_on_success_json":row.runtime_policy_on_success_json,
            "checkpoint_json":row.checkpoint_json,"created_at_us":row.created_at_us,
            "updated_at_us":row.updated_at_us,"terminal_at_us":row.terminal_at_us,
        })).map_err(storage)
    })).await.unwrap()
}

async fn lease_bytes(store: &ToastyUserAppStore, operation_id: &str) -> Vec<u8> {
    let id = operation_id.to_owned();
    store.run(true, move |tx,_| Box::pin(async move {
        let row=models::OperationLease::get_by_operation_id(tx,&id).await.map_err(storage)?;
        serde_json::to_vec(&serde_json::json!({
            "operation_id":row.operation_id,"app_id":row.app_id,"lifecycle_id":row.lifecycle_id,
            "executor_id":row.executor_id,"request_fingerprint":row.request_fingerprint,
            "receipt_version":row.receipt_version,"receipt_json":row.receipt_json,"created_at_us":row.created_at_us,
        })).map_err(storage)
    })).await.unwrap()
}

async fn operation_pages(store: &ToastyUserAppStore, full_bad_pages: bool) {
    let prefix = format!("zzscan{}", uuid::Uuid::new_v4().simple());
    let mut bad = Vec::new();
    let count = if full_bad_pages { 6 } else { 3 };
    for index in 0..count {
        let (operation, context, _) = seed(store, format!("{prefix}-000bad{index}"), false).await;
        let field = ["command-schema", "checkpoint", "fingerprint"][index % 3];
        corrupt_operation(store, &operation, field).await;
        let bytes = operation_bytes(store, &operation.operation_id).await;
        assert!(
            store
                .get_operation(&operation.app_id, &operation.operation_id)
                .await
                .is_err()
        );
        assert!(store.check_business_execution(&context).await.is_err());
        assert!(
            store
                .advance(&progress(&operation, UserAppOperationState::Running))
                .await
                .is_err()
        );
        bad.push((operation, bytes));
    }
    let (good, _, _) = seed(store, format!("{prefix}-900good"), false).await;
    let mut cursor = Some(prefix.clone());
    let mut found = None;
    for page_index in 0..10 {
        let page = store
            .scan_unfinished_operations(cursor.as_deref(), 2)
            .await
            .unwrap();
        if page_index < count / 2 {
            assert!(
                page.records.is_empty(),
                "full bad raw pages contain no executable records"
            );
            assert!(!page.exhausted, "empty decoded page is not raw EOF");
        }
        if let Some(record) = page
            .records
            .into_iter()
            .find(|record| record.operation_id == good.operation_id)
        {
            found = Some(record);
            break;
        }
        let next = page
            .next_after
            .expect("non-exhausted raw page needs a progress cursor");
        assert!(
            Some(&next) > cursor.as_ref(),
            "cursor must advance beyond undecodable history"
        );
        cursor = Some(next);
    }
    let recovered =
        found.expect("independent valid B must remain reachable after malformed A pages");
    assert_eq!(recovered, good);
    let running = store
        .advance(&progress(&recovered, UserAppOperationState::Running))
        .await
        .unwrap();
    assert_eq!(running.state, UserAppOperationState::Running);
    for (operation, before) in bad {
        assert_eq!(
            operation_bytes(store, &operation.operation_id).await,
            before
        );
        assert!(
            store
                .get_operation(&operation.app_id, &operation.operation_id)
                .await
                .is_err()
        );
    }
}

async fn terminal_pages(store: &ToastyUserAppStore) {
    let prefix = format!("zzlease{}", uuid::Uuid::new_v4().simple());
    let mut bad = Vec::new();
    for index in 0..9 {
        let (operation, context, binding) =
            seed(store, format!("{prefix}-000bad{index}"), true).await;
        if index < 6 {
            corrupt_lease(
                store,
                &operation,
                ["receipt-schema", "receipt", "fingerprint"][index % 3],
            )
            .await;
        } else {
            corrupt_operation(
                store,
                &operation,
                ["command-schema", "checkpoint", "fingerprint"][index % 3],
            )
            .await;
        }
        let operation_before = operation_bytes(store, &operation.operation_id).await;
        let lease_before = lease_bytes(store, &operation.operation_id).await;
        if index < 6 {
            assert!(
                store
                    .get_operation_lease(&operation.app_id, &operation.operation_id)
                    .await
                    .is_err()
            );
        } else {
            assert!(
                store
                    .get_operation(&operation.app_id, &operation.operation_id)
                    .await
                    .is_err()
            );
        }
        assert!(store.check_business_execution(&context).await.is_err());
        assert!(
            store
                .forget_operation_lease(&binding.unwrap())
                .await
                .is_err()
        );
        bad.push((operation, operation_before, lease_before));
    }
    let (good, _, binding) = seed(store, format!("{prefix}-900good"), true).await;
    let binding = binding.unwrap();
    let mut cursor = Some(prefix);
    let mut found = None;
    for page_index in 0..10 {
        let page = store
            .scan_terminal_operation_leases(cursor.as_deref(), 2)
            .await
            .unwrap();
        if page_index < 4 {
            assert!(page.records.is_empty());
            assert!(!page.exhausted);
        }
        if let Some(record) = page
            .records
            .into_iter()
            .find(|record| record.context.operation_id == good.operation_id)
        {
            found = Some(record);
            break;
        }
        let next = page.next_after.expect("raw terminal page must advance");
        assert!(Some(&next) > cursor.as_ref());
        cursor = Some(next);
    }
    assert_eq!(
        found,
        Some(binding.clone()),
        "bad A receipts cannot hide valid B cleanup identity"
    );
    store.forget_operation_lease(&binding).await.unwrap();
    assert!(
        store
            .get_operation_lease(&good.app_id, &good.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    for (operation, operation_before, lease_before) in bad {
        assert_eq!(
            operation_bytes(store, &operation.operation_id).await,
            operation_before
        );
        assert_eq!(
            lease_bytes(store, &operation.operation_id).await,
            lease_before
        );
        assert!(
            store
                .forget_operation_lease(&UserAppOperationLeaseBinding {
                    context: UserAppExecutionContext {
                        app_id: operation.app_id.clone(),
                        lifecycle_id: operation.lifecycle_id.clone(),
                        operation_id: operation.operation_id.clone(),
                        executor_id: "scan-fixture-executor".into(),
                        request_fingerprint: operation.request_fingerprint.clone(),
                    },
                    receipt: UserAppOperationLeaseReceipt::Docker {
                        service_type: ServiceType::Userapp,
                        device: 1,
                        inode: 42,
                        token: operation.operation_id.clone(),
                    },
                })
                .await
                .is_err()
        );
    }
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn turso_recovery_scan_preserves_bad_rows_and_advances_full_bad_pages() {
    let directory = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&directory.path().join("recovery-scan.db"))
        .await
        .unwrap();
    operation_pages(&store, false).await;
    operation_pages(&store, true).await;
    terminal_pages(&store).await;
    crate::userapp_lifecycle::UserAppStoreControl::shutdown(&store)
        .await
        .unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL test database"]
async fn pg_recovery_scan_preserves_bad_rows_and_advances_full_bad_pages() {
    let dsn = std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG DSN required");
    let config = crate::config::PostgresConfig {
        url: Some(dsn),
        max_connections: Some(1),
        min_connections: Some(1),
        ..Default::default()
    };
    let store = ToastyUserAppStore::connect(&config).await.unwrap();
    operation_pages(&store, false).await;
    operation_pages(&store, true).await;
    terminal_pages(&store).await;
    crate::userapp_lifecycle::UserAppStoreControl::shutdown(&store)
        .await
        .unwrap();
}
