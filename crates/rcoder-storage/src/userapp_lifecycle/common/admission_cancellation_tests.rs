//! Cancellation after complete real admission must not cancel the owned transaction.
use super::{ToastyUserAppStore, repo};
use shared_types::*;
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;

#[derive(Default)]
pub(super) struct AdmissionGate {
    entered: Notify,
    release: Notify,
}
impl AdmissionGate {
    pub(super) async fn pause_before_commit(&self) {
        self.entered.notify_one();
        tokio::time::timeout(Duration::from_secs(10), self.release.notified())
            .await
            .expect("test must release admitted transaction");
    }
}

async fn cancel_admission(store: ToastyUserAppStore) -> (UserAppAdmission, UserAppExecutionInput) {
    let mut store = store;
    let gate = Arc::new(AdmissionGate::default());
    store.admission_gate = Some(gate.clone());
    let store = Arc::new(store);
    let app = store
        .ensure_identity(&format!("cancel{}", uuid::Uuid::new_v4().simple()))
        .await
        .unwrap();
    let input = UserAppExecutionInput::new("{\"fixture\":\"cancelled-private-input\"}".into());
    let request = UserAppAdmission {
        app_id: app.app_id,
        lifecycle_id: Some(app.lifecycle_id),
        operation_id: uuid::Uuid::new_v4().simple().to_string(),
        request_id: Some(uuid::Uuid::new_v4().simple().to_string()),
        request_fingerprint: input.digest(),
        kind: UserAppOperationKind::Create,
        command: Some(UserAppControlCommand::Create {
            input_digest: input.digest(),
        }),
        metadata: None,
        runtime_policy_on_success: None,
    };
    let task = tokio::spawn({
        let (store, request, input) = (store.clone(), request.clone(), input.clone());
        async move { store.admit_with_input(&request, Some(&input)).await }
    });
    // This signal is emitted inside the actual admission transaction, after all
    // four records were written but before COMMIT, never merely after queueing.
    tokio::time::timeout(Duration::from_secs(10), gate.entered.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    // If cancellation had aborted the owner task, this shutdown would finish
    // without committing. The independent observer below catches that failure.
    gate.release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), store.shutdown())
        .await
        .unwrap()
        .unwrap();
    (request, input)
}

async fn verify_complete(
    store: &ToastyUserAppStore,
    request: &UserAppAdmission,
    input: &UserAppExecutionInput,
) {
    let original = store
        .get_operation_by_request(&request.app_id, request.request_id.as_deref().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.operation_id, request.operation_id);
    assert_eq!(Some(&original.lifecycle_id), request.lifecycle_id.as_ref());
    assert_eq!(original.state, UserAppOperationState::Pending);
    let app = store
        .get_application(&request.app_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        app.active_operations.prod.as_deref(),
        Some(request.operation_id.as_str())
    );
    assert!(app.active_operations.dev.is_none() && app.active_operations.application.is_none());
    let (app_id, operation_id, request_id, payload) = (
        request.app_id.clone(),
        request.operation_id.clone(),
        request.request_id.clone().unwrap(),
        input.encoded().to_owned(),
    );
    let backend = store.backend;
    store
        .owner
        .execute(move |mut db| async move {
            for (table, field, expected) in [
                ("userapp_operations", "operation_id", operation_id.clone()),
                ("userapp_requests", "request_id", request_id),
                ("userapp_operation_inputs", "payload", payload),
                (
                    "userapp_active_operations",
                    "prod_operation_id",
                    operation_id,
                ),
            ] {
                let rows = toasty::sql::query(repo::sql(
                    backend,
                    &format!("SELECT {field} FROM {table} WHERE app_id=$1"),
                ))
                .bind(app_id.clone())
                .exec(&mut db)
                .await?;
                assert_eq!(
                    repo::strings(rows).unwrap(),
                    vec![expected],
                    "one complete {table} record"
                );
            }
            Ok(())
        })
        .await
        .unwrap();
    match store.admit_with_input(request, Some(input)).await.unwrap() {
        UserAppAdmissionOutcome::Existing(record) => {
            assert_eq!(record.operation_id, request.operation_id)
        }
        other => panic!("original request must replay, got {other:?}"),
    }
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn turso_cancelled_admission_commits_complete_original_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cancel.db");
    let (request, input) =
        cancel_admission(ToastyUserAppStore::open_exclusive(&path).await.unwrap()).await;
    // Turso intentionally forbids concurrent process owners; reopen independently
    // only after the first owner drained and released the real directory lock.
    let observer = ToastyUserAppStore::open_exclusive(&path).await.unwrap();
    verify_complete(&observer, &request, &input).await;
    observer.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires RCODER_USERAPP_PG_TEST_DSN for an isolated PostgreSQL instance"]
async fn pg_cancelled_admission_commits_complete_original_request() {
    let config = crate::config::PostgresConfig {
        url: Some(
            std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("explicit isolated PG DSN required"),
        ),
        ..Default::default()
    };
    let owner = ToastyUserAppStore::connect(&config).await.unwrap();
    let observer = ToastyUserAppStore::connect(&config).await.unwrap();
    let (request, input) = cancel_admission(owner).await;
    verify_complete(&observer, &request, &input).await;
    observer.shutdown().await.unwrap();
}
