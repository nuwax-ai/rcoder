//! Historical stop diagnostics stay unchanged across one exact completion transaction.
use super::*;

async fn raw_checkpoint(store: &ToastyUserAppStore, operation_id: &str) -> String {
    let operation_id = operation_id.to_owned();
    store
        .run(false, move |tx, backend| {
            let operation_id = operation_id.clone();
            Box::pin(async move {
                let rows = toasty::sql::query(super::super::repo::sql(
                    backend,
                    "SELECT checkpoint_json FROM userapp_compute_controls WHERE operation_id=$1",
                ))
                .bind(operation_id)
                .exec(tx)
                .await
                .map_err(super::super::storage)?;
                let mut rows = super::super::repo::strings(rows)?;
                assert_eq!(rows.len(), 1);
                Ok(rows.remove(0))
            })
        })
        .await
        .unwrap()
}

async fn observed_stop_history_contract(store: &ToastyUserAppStore) {
    for shape in 0..5 {
        for recovery_required in [false, true] {
            let app = store
                .ensure_identity(&format!("observedstop{}", uuid::Uuid::new_v4().simple()))
                .await
                .unwrap();
            let mut input = request(
                &app,
                &format!("stop{}", uuid::Uuid::new_v4().simple()),
                ComputeControlAction::Stop,
            );
            if shape >= 3 {
                input.scope = UserAppOperationScope::Prod;
            }
            let pending = store.admit_compute_control(&input).await.unwrap();
            let identity = compute_identity(&pending);
            store
                .claim_compute_control(&identity, pending.revision)
                .await
                .unwrap();
            let mut lease = compute_receipt();
            if shape >= 3
                && let UserAppOperationLeaseReceipt::Kubernetes { service_type, .. } = &mut lease
            {
                *service_type = ServiceType::Userapp;
            }
            let mut original = store.bind_compute_lease(&identity, &lease).await.unwrap();
            let context = original.execution_context().unwrap();
            let resource = AppResourceIdentity {
                kind: AppResourceKind::StatefulSet,
                name: "original-builder".into(),
                uid: "original-sts-uid".into(),
                resource_version: Some("10".into()),
            };
            let checkpoint = match shape {
                0 | 3 => serde_json::to_value(ComputeAbsenceCheckpoint {
                    context,
                    compute_absent: true,
                })
                .unwrap(),
                1 => serde_json::to_value(BuilderOrphanStopTarget {
                    context,
                    resource_binding: None,
                    orphan_pod: BuilderPodIdentity {
                        name: "original-builder-0".into(),
                        uid: "original-pod-uid".into(),
                        resource_version: "11".into(),
                    },
                    controller_name: resource.name,
                    controller_uid: resource.uid,
                })
                .unwrap(),
                2 => serde_json::to_value(BuilderControlTarget {
                    context,
                    resource_binding: None,
                    workload: Some(resource),
                    pod: None,
                    restart_image: None,
                    restart_runtime_workspace: None,
                    builder_startup_mode: BuilderStartupMode::default(),
                })
                .unwrap(),
                4 => serde_json::to_value(UserAppMutationTarget {
                    context,
                    resource: AppResourceIdentity {
                        kind: AppResourceKind::Deployment,
                        ..resource
                    },
                })
                .unwrap(),
                _ => unreachable!(),
            };
            let mut progress = compute_progress(&original, ComputeControlStage::Stopping);
            progress.checkpoint = checkpoint;
            if recovery_required {
                progress.state = ComputeControlState::RecoveryRequired;
                progress.error_code = Some("LOST_REPLY".into());
                progress.error_message =
                    Some("original stop completed after its reply was lost".into());
            }
            original = store.advance_compute_control(&progress).await.unwrap();
            let mut historical = original.checkpoint.clone();
            historical["old_diagnostic"] = serde_json::json!({"owner_epoch":"old", "opaque_text":" unchanged bytes ", "old_version":4});
            if matches!(shape, 0 | 3 | 4) {
                historical["resource_binding"] = "retired metadata from an older version".into();
            }
            if matches!(shape, 1 | 2 | 4) {
                historical["compute_absent"] = false.into();
            }
            if shape != 1 {
                historical["controller_name"] = serde_json::json!({"old_format":4});
                historical["controller_uid"] = "retired controller metadata".into();
            }
            let payload = serde_json::to_string(&historical).unwrap();
            let operation = original.operation_id.clone();
            let saved_payload = payload.clone();
            store.run(false, move |tx, backend| {
                let operation = operation.clone(); let payload = saved_payload.clone();
                Box::pin(async move {
                    toasty::sql::statement(super::super::repo::sql(backend,
                        "UPDATE userapp_compute_controls SET checkpoint_json=$2 WHERE operation_id=$1"))
                        .bind(operation).bind(payload).exec(tx).await.map_err(super::super::storage)?;
                    Ok(())
                })
            }).await.unwrap();
            original = store
                .get_compute_control(&original.app_id, &original.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(raw_checkpoint(store, &original.operation_id).await, payload);

            for variant in 0..5 {
                let mut snapshot = original.clone();
                match variant {
                    0 => snapshot.checkpoint["context"]["app_id"] = "foreign-app".into(),
                    1 => snapshot.revision += 1,
                    2 => {
                        if let Some(UserAppOperationLeaseReceipt::Kubernetes { uid, .. }) =
                            &mut snapshot.lease
                        {
                            *uid = "replacement-lease".into();
                        }
                    }
                    3 => match shape {
                        0 | 3 => snapshot.checkpoint["compute_absent"] = false.into(),
                        1 => snapshot.checkpoint["orphan_pod"]["uid"] = "different-pod".into(),
                        2 => snapshot.checkpoint["workload"]["uid"] = "different-sts".into(),
                        4 => snapshot.checkpoint["resource"]["uid"] = "different-deployment".into(),
                        _ => unreachable!(),
                    },
                    4 => match shape {
                        0 | 3 => {
                            snapshot.checkpoint["resource"] = serde_json::json!({"kind":"Deployment","name":"unexpected","uid":"different-uid","resource_version":"1"})
                        }
                        _ => snapshot.checkpoint["compute_absent"] = true.into(),
                    },
                    _ => unreachable!(),
                }
                assert!(
                    store
                        .finalize_observed_compute_stop(&snapshot)
                        .await
                        .is_err()
                );
                assert_eq!(
                    store
                        .get_compute_control(&original.app_id, &original.operation_id)
                        .await
                        .unwrap(),
                    Some(original.clone())
                );
                assert_eq!(raw_checkpoint(store, &original.operation_id).await, payload);
            }
            let (first, second) = tokio::join!(
                store.finalize_observed_compute_stop(&original),
                store.finalize_observed_compute_stop(&original)
            );
            let completed = match (first, second) {
                (Ok(record), Err(UserAppStoreError::VersionConflict))
                | (Err(UserAppStoreError::VersionConflict), Ok(record)) => record,
                other => panic!("one original stop completion CAS must win: {other:?}"),
            };
            assert_eq!(completed.state, ComputeControlState::Succeeded);
            assert_eq!(completed.stage, "completed");
            assert_eq!(completed.checkpoint, original.checkpoint);
            assert_eq!(completed.lease, original.lease);
            assert_eq!(completed.operation_id, original.operation_id);
            assert_eq!(completed.executor_id, original.executor_id);
            assert_eq!(completed.generation, original.generation);
            assert_eq!(completed.request_id, original.request_id);
            assert_eq!(completed.request_fingerprint, original.request_fingerprint);
            assert_eq!(raw_checkpoint(store, &original.operation_id).await, payload);
        }
    }
}

#[tokio::test]
async fn observed_stop_history_completion_preserves_original_bytes_and_atomic_cas() {
    let root = tempfile::tempdir().unwrap();
    let store = ToastyUserAppStore::open_exclusive(&root.path().join("observed-stop.db"))
        .await
        .unwrap();
    observed_stop_history_contract(&store).await;
    store.shutdown().await.unwrap();
}

#[cfg(feature = "pg")]
#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL database"]
async fn observed_stop_history_pg_completion_preserves_original_bytes_and_atomic_cas() {
    let config = crate::config::PostgresConfig {
        url: Some(std::env::var("RCODER_USERAPP_PG_TEST_DSN").expect("disposable PG required")),
        max_connections: Some(2),
        min_connections: Some(1),
        statement_timeout_secs: Some(5),
        ..Default::default()
    };
    let store = ToastyUserAppStore::connect(&config).await.unwrap();
    observed_stop_history_contract(&store).await;
    store.shutdown().await.unwrap();
}
