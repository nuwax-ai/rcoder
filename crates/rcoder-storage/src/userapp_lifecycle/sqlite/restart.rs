//! Only the exclusive single-process startup path may classify its old writers.
//! This records uncertainty; it never steals a resource lease or replays a write.
use super::SqliteUserAppStore;
use crate::userapp_lifecycle::storage;
use shared_types::{
    UserAppOperationRecord, UserAppOperationState as State, UserAppStoreError as Error,
};

pub(super) async fn quarantine(store: &SqliteUserAppStore) -> Result<(), Error> {
    let mut tx = store
        .pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(storage)?;
    let records: Vec<String> = sqlx::query_scalar(
        "SELECT record FROM userapp_operations WHERE terminal=0 AND json_extract(record, '$.state') IN ('Running','WaitingRetry')",
    ).fetch_all(&mut *tx).await.map_err(storage)?;
    let count = records.len();
    for original in records {
        let mut record: UserAppOperationRecord =
            serde_json::from_str(&original).map_err(storage)?;
        record.state = State::RecoveryRequired;
        record.revision = record.revision.checked_add(1).ok_or_else(|| {
            Error::InvalidOperation("Operation revision exhausted during restart".into())
        })?;
        record.error_code = Some(shared_types::error_codes::ERR_BACKEND_ERROR.into());
        record.error_message =
            Some("Previous SQLite executor stopped; remote outcome requires verification".into());
        let encoded = serde_json::to_string(&record).map_err(storage)?;
        let updated = sqlx::query("UPDATE userapp_operations SET record=$1 WHERE operation_id=$2 AND record=$3 AND terminal=0")
            .bind(encoded).bind(&record.operation_id).bind(original)
            .execute(&mut *tx).await.map_err(storage)?;
        if updated.rows_affected() != 1 {
            return Err(Error::VersionConflict);
        }
    }
    tx.commit().await.map_err(storage)?;
    if count != 0 {
        tracing::warn!(
            operations = count,
            "Interrupted SQLite operations require remote outcome verification"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::{
        UserAppAdmission, UserAppAdmissionOutcome, UserAppLifecycleStore as _,
        UserAppOperationKind, UserAppOperationProgress,
    };

    async fn admitted(store: &SqliteUserAppStore, app: &str) -> UserAppOperationRecord {
        let outcome = store
            .admit(&UserAppAdmission {
                app_id: app.into(),
                lifecycle_id: None,
                operation_id: format!("operation-{app}"),
                request_id: Some(format!("request-{app}")),
                request_fingerprint: "a".repeat(64),
                kind: UserAppOperationKind::Stop,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            })
            .await
            .expect("admit");
        match outcome {
            UserAppAdmissionOutcome::Accepted(record) => record,
            _ => panic!("new operation"),
        }
    }

    async fn advance(
        store: &SqliteUserAppStore,
        record: &UserAppOperationRecord,
        state: State,
    ) -> UserAppOperationRecord {
        store
            .advance(&UserAppOperationProgress {
                app_id: record.app_id.clone(),
                operation_id: record.operation_id.clone(),
                lifecycle_id: record.lifecycle_id.clone(),
                expected_revision: record.revision,
                executor_id: "previous-worker".into(),
                state,
                step: "captured".into(),
                checkpoint: serde_json::json!({"uid":"original-resource"}),
                error_code: None,
                error_message: None,
            })
            .await
            .expect("advance")
    }

    #[tokio::test]
    async fn exclusive_restart_quarantines_only_interrupted_claims_without_replaying() {
        let directory = tempfile::tempdir().expect("fixture");
        let path = directory.path().join("userapp.sqlite3");
        let store = SqliteUserAppStore::open_exclusive(&path)
            .await
            .expect("exclusive store");
        let running = advance(&store, &admitted(&store, "running").await, State::Running).await;
        let pending = admitted(&store, "pending").await;
        let completed = advance(&store, &admitted(&store, "completed").await, State::Running).await;
        let completed = advance(&store, &completed, State::Succeeded).await;
        let marker = directory.path().join("external-operation-marker");
        std::fs::write(&marker, &running.operation_id).expect("remote ownership marker");
        store.close().await;
        drop(store);
        let restarted = SqliteUserAppStore::open_exclusive(&path)
            .await
            .expect("restart");
        let observed = restarted
            .get_operation(&running.app_id, &running.operation_id)
            .await
            .expect("query")
            .expect("record");
        assert_eq!(observed.state, State::RecoveryRequired);
        assert_eq!(observed.revision, running.revision + 1);
        assert_eq!(observed.checkpoint, running.checkpoint);
        assert_eq!(observed.executor_id, running.executor_id);
        assert_eq!(observed.step, running.step);
        assert_eq!(
            restarted
                .get_application(&running.app_id)
                .await
                .expect("application")
                .expect("identity")
                .active_operations
                .prod,
            Some(running.operation_id.clone())
        );
        for original in [&pending, &completed] {
            assert_eq!(
                restarted
                    .get_operation(&original.app_id, &original.operation_id)
                    .await
                    .expect("query")
                    .as_ref(),
                Some(original)
            );
        }
        assert_eq!(
            std::fs::read_to_string(&marker).expect("retained marker"),
            running.operation_id
        );
        restarted.close().await;
        drop(restarted);
        let again = SqliteUserAppStore::open_exclusive(&path)
            .await
            .expect("second restart");
        assert_eq!(
            again
                .get_operation(&running.app_id, &running.operation_id)
                .await
                .expect("query")
                .as_ref(),
            Some(&observed),
            "quarantine must not bump revisions on every restart"
        );
        again.close().await;
    }
    #[tokio::test]
    async fn invalid_interrupted_record_blocks_startup_without_partial_quarantine() {
        let directory = tempfile::tempdir().expect("fixture");
        let path = directory.path().join("userapp.sqlite3");
        let store = SqliteUserAppStore::open_exclusive(&path)
            .await
            .expect("store");
        let running = advance(&store, &admitted(&store, "valid").await, State::Running).await;
        let broken = admitted(&store, "invalid").await;
        let malformed = serde_json::json!({"state":"Running", "revision":"invalid"}).to_string();
        sqlx::query("UPDATE userapp_operations SET record=$1 WHERE operation_id=$2")
            .bind(&malformed)
            .bind(&broken.operation_id)
            .execute(&store.pool)
            .await
            .expect("inject corrupt durable record");
        store.close().await;
        drop(store);
        assert!(
            SqliteUserAppStore::open_exclusive(&path).await.is_err(),
            "startup must fail instead of hiding corrupt recovery state"
        );
        let inspect = SqliteUserAppStore::open(&path)
            .await
            .expect("inspect database");
        let preserved = inspect
            .get_operation(&running.app_id, &running.operation_id)
            .await
            .expect("query valid record")
            .expect("record");
        assert_eq!(
            preserved, running,
            "the whole quarantine transaction must roll back"
        );
        let original: String =
            sqlx::query_scalar("SELECT record FROM userapp_operations WHERE operation_id=$1")
                .bind(&broken.operation_id)
                .fetch_one(&inspect.pool)
                .await
                .expect("raw record");
        assert_eq!(original, malformed, "corruption is retained for diagnosis");
        inspect.close().await;
    }
}
