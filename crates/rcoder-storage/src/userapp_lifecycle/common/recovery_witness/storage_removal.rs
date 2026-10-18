//! Existing destroy/clear operations retire only resources their receipts remove.
use super::*;

async fn completed(
    store: &ToastyUserAppStore,
    found: &UserAppDiscoveredIdentity,
    kind: UserAppOperationKind,
) -> UserAppOperationRecord {
    let command = match kind {
        UserAppOperationKind::DestroyDevStorage => {
            UserAppControlCommand::DestroyStorage { production: false }
        }
        UserAppOperationKind::DestroyProdStorage => {
            UserAppControlCommand::DestroyStorage { production: true }
        }
        UserAppOperationKind::ClearProdStorage => {
            UserAppControlCommand::ClearStorage { production: true }
        }
        _ => panic!("fixture requires an explicit storage-removal kind"),
    };
    let UserAppAdmissionOutcome::Accepted(mut operation) = store
        .admit(&UserAppAdmission {
            app_id: found.app_id.clone(),
            lifecycle_id: Some(found.lifecycle_id.clone()),
            operation_id: uuid::Uuid::new_v4().to_string(),
            request_id: None,
            request_fingerprint: "a".repeat(64),
            kind,
            command: Some(command),
            metadata: None,
            runtime_policy_on_success: None,
        })
        .await
        .unwrap()
    else {
        panic!("fresh storage operation");
    };
    let progress = |operation: &UserAppOperationRecord, state, step: &str, checkpoint| {
        UserAppOperationProgress {
            app_id: operation.app_id.clone(),
            lifecycle_id: operation.lifecycle_id.clone(),
            operation_id: operation.operation_id.clone(),
            expected_revision: operation.revision,
            executor_id: "original-storage-executor".into(),
            state,
            step: step.into(),
            checkpoint,
            error_code: None,
            error_message: None,
        }
    };
    operation = store
        .advance(&progress(
            &operation,
            UserAppOperationState::Running,
            "claimed",
            serde_json::Value::Null,
        ))
        .await
        .unwrap();
    let context = UserAppExecutionContext {
        app_id: found.app_id.clone(),
        lifecycle_id: found.lifecycle_id.clone(),
        operation_id: operation.operation_id.clone(),
        executor_id: "original-storage-executor".into(),
        request_fingerprint: operation.request_fingerprint.clone(),
    };
    let family = match kind.scope() {
        UserAppOperationScope::Dev => ServiceType::UserappBuilder,
        UserAppOperationScope::Prod => ServiceType::Userapp,
        UserAppOperationScope::Application => panic!("storage scope required"),
    };
    store
        .bind_operation_lease(
            &context,
            &UserAppOperationLeaseReceipt::Docker {
                service_type: family,
                device: 1,
                inode: 2,
                token: operation.operation_id.clone(),
            },
        )
        .await
        .unwrap();
    let production = AppDeletionSnapshot {
        app_id: found.app_id.clone(),
        operation_id: "captured-production".into(),
        resources: found.prod_volumes.clone(),
        directories: None,
    };
    let (checkpoint, captured_step, complete_step) = match kind {
        UserAppOperationKind::DestroyDevStorage | UserAppOperationKind::DestroyProdStorage => {
            let receipt = UserAppStorageDestruction {
                context,
                production: (kind == UserAppOperationKind::DestroyProdStorage)
                    .then_some(production),
                development: UserappDevDeletionReceipt {
                    runtime: BuilderDeletionSnapshot {
                        app_id: found.app_id.clone(),
                        operation_id: "captured-development".into(),
                        resources: found.dev_volumes.clone(),
                        docker_bind_cleanup: false,
                        resource_binding: None,
                    },
                    registry: None,
                    lease: None,
                    collaborators: Vec::new(),
                    directories: None,
                },
            };
            (
                serde_json::to_value(receipt).unwrap(),
                "storage_captured",
                UserAppStorageDestruction::COMPLETE_STEP,
            )
        }
        UserAppOperationKind::ClearProdStorage => {
            let receipt = UserAppStorageClear {
                context,
                target: UserAppStorageClearTarget::Production {
                    snapshot: production,
                    directories: Vec::new(),
                },
            };
            (
                serde_json::to_value(receipt).unwrap(),
                "clear_target_captured",
                UserAppStorageClear::COMPLETE_STEP,
            )
        }
        _ => panic!("fixture requires a storage-removal kind"),
    };
    operation = store
        .advance(&progress(
            &operation,
            UserAppOperationState::Running,
            captured_step,
            checkpoint.clone(),
        ))
        .await
        .unwrap();
    assert!(
        store
            .advance(&progress(
                &operation,
                UserAppOperationState::Succeeded,
                "completed",
                checkpoint.clone()
            ))
            .await
            .is_err(),
        "captured resources cannot be reported successful before the final confirmed removal stage"
    );
    if kind == UserAppOperationKind::DestroyProdStorage {
        operation = store
            .advance(&progress(
                &operation,
                UserAppOperationState::Running,
                "production_storage_removed",
                checkpoint.clone(),
            ))
            .await
            .unwrap();
    }
    operation = store
        .advance(&progress(
            &operation,
            UserAppOperationState::Running,
            complete_step,
            checkpoint.clone(),
        ))
        .await
        .unwrap();
    store
        .advance(&progress(
            &operation,
            UserAppOperationState::Succeeded,
            "completed",
            checkpoint,
        ))
        .await
        .unwrap()
}

pub(super) async fn contract(store: &ToastyUserAppStore) {
    for (n, kind) in [
        UserAppOperationKind::DestroyDevStorage,
        UserAppOperationKind::DestroyProdStorage,
        UserAppOperationKind::ClearProdStorage,
    ]
    .into_iter()
    .enumerate()
    {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let found = UserAppDiscoveredIdentity {
            app_id: format!("storage-removal-{n}-{suffix}"),
            lifecycle_id: format!("lifecycle-{suffix}"),
            dev_uid: Some("old-builder".into()),
            prod_uid: Some("old-app".into()),
            dev_stopped: false,
            prod_stopped: false,
            dev_volumes: vec![pvc("dev", "original-dev-uid")],
            prod_volumes: vec![pvc("prod", "original-prod-uid")],
        };
        store.restore_discovered_identity(&found).await.unwrap();
        let operation = completed(store, &found, kind).await;
        let before = raw(store, &found.app_id).await;
        let remaining = store
            .get_recovery_storage_witness(&found.app_id, &found.lifecycle_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            remaining.dev_volumes.is_empty(),
            kind != UserAppOperationKind::ClearProdStorage
        );
        assert_eq!(
            remaining.prod_volumes.is_empty(),
            kind != UserAppOperationKind::DestroyDevStorage
        );
        assert_eq!(
            store
                .get_recovery_witness(&found.app_id, &found.lifecycle_id)
                .await
                .unwrap(),
            Some(found.clone())
        );
        assert_eq!(raw(store, &found.app_id).await, before);
        for state in [
            UserAppOperationState::Failed,
            UserAppOperationState::RecoveryRequired,
        ] {
            let mut rejected = operation.clone();
            rejected.state = state;
            let mut projected = found.clone();
            assert!(subtract_confirmed_retirement(&mut projected, &rejected).is_err());
            assert_eq!(projected, found);
        }
        let mut foreign = operation.clone();
        foreign.lifecycle_id = "foreign-lifecycle".into();
        let mut incomplete = operation.clone();
        incomplete.state = UserAppOperationState::Running;
        incomplete.step = "storage_captured".into();
        let mut wrong_context = operation.clone();
        wrong_context.executor_id = Some("foreign-executor".into());
        for rejected in [foreign, incomplete, wrong_context] {
            let mut projected = found.clone();
            assert!(subtract_confirmed_retirement(&mut projected, &rejected).is_err());
            assert_eq!(projected, found);
        }
        let mut newer = found.clone();
        newer.dev_volumes[0].uid = "new-dev-uid".into();
        newer.prod_volumes[0].uid = "new-prod-uid".into();
        let expected = newer.clone();
        subtract_confirmed_retirement(&mut newer, &operation).unwrap();
        assert_eq!(
            newer, expected,
            "same name with new physical UID stays protected"
        );
        if kind == UserAppOperationKind::ClearProdStorage {
            let mut docker_like = operation.clone();
            let mut receipt: UserAppStorageClear =
                serde_json::from_value(docker_like.checkpoint.clone()).unwrap();
            if let UserAppStorageClearTarget::Production { directories, .. } = &mut receipt.target {
                directories.push(storage_contents::CapturedStorageDirectory {
                    path: std::env::temp_dir().join("preserved-clear-directory"),
                    identity: None,
                });
            }
            docker_like.checkpoint = serde_json::to_value(receipt).unwrap();
            let mut projected = found.clone();
            subtract_confirmed_retirement(&mut projected, &docker_like).unwrap();
            assert_eq!(projected, found, "directory clear retains its volumes");
            let mut development_clear = operation.clone();
            development_clear.kind = UserAppOperationKind::ClearDevStorage;
            development_clear.scope = UserAppOperationScope::Dev;
            assert!(subtract_confirmed_retirement(&mut projected, &development_clear).is_err());
            assert_eq!(projected, found);
        }
    }
}
