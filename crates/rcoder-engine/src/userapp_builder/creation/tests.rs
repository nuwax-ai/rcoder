use super::*;

#[cfg(test)]
mod cases {
    use super::*;
    #[tokio::test]
    async fn expired_creation_retains_observation_and_discards_late_success() {
        let local = crate::userapp_builder::lifecycle::acquire("budget-observation").await;
        let (complete, result) = tokio::sync::oneshot::channel();
        let (recorded, checkpoint) = tokio::sync::oneshot::channel();
        let (expire, expiry) = tokio::sync::oneshot::channel();
        let (started, start) = tokio::sync::oneshot::channel();
        let work = async move {
            let _local = local;
            observe_creation_budget(
                async {
                    expiry.await.expect("expiry");
                },
                async {
                    started.send(()).expect("start observer");
                    result.await.map_err(anyhow::Error::from)
                },
                async {
                    recorded.send(()).expect("observer");
                    Ok(())
                },
            )
            .await
        };
        let worker = tokio::spawn(work);
        tokio::time::timeout(Duration::from_secs(1), start)
            .await
            .expect("start deadline")
            .expect("started");
        expire.send(()).expect("expire active operation");
        tokio::time::timeout(Duration::from_secs(1), checkpoint)
            .await
            .expect("checkpoint deadline")
            .expect("recorded");
        assert!(!worker.is_finished());
        assert!(
            crate::userapp_builder::lifecycle::try_acquire("budget-observation")
                .await
                .is_none()
        );
        complete.send("late resource").expect("still observed");
        assert!(
            tokio::time::timeout(Duration::from_secs(1), worker)
                .await
                .expect("worker deadline")
                .expect("worker")
                .expect("observation")
                .is_none()
        );
        assert!(
            crate::userapp_builder::lifecycle::try_acquire("budget-observation")
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn expired_checkpoint_failure_still_observes_remote_completion() {
        let (complete, result) = tokio::sync::oneshot::channel::<()>();
        let (recorded, checkpoint) = tokio::sync::oneshot::channel();
        let (expire, expiry) = tokio::sync::oneshot::channel();
        let (started, start) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(observe_creation_budget(
            async {
                expiry.await.expect("expiry");
            },
            async {
                started.send(()).expect("start observer");
                result.await.map_err(anyhow::Error::from)
            },
            async {
                recorded.send(()).expect("observer");
                Err(anyhow!("database unavailable"))
            },
        ));
        tokio::time::timeout(Duration::from_secs(1), start)
            .await
            .expect("start deadline")
            .expect("started");
        expire.send(()).expect("expire active operation");
        tokio::time::timeout(Duration::from_secs(1), checkpoint)
            .await
            .expect("checkpoint deadline")
            .expect("attempted");
        assert!(!worker.is_finished());
        complete.send(()).expect("still observed");
        assert!(
            tokio::time::timeout(Duration::from_secs(1), worker)
                .await
                .expect("worker deadline")
                .expect("worker")
                .is_err()
        );
    }

    #[tokio::test]
    async fn expired_budget_never_starts_unpolled_creation() {
        let result = observe_creation_budget::<()>(
            std::future::ready(()),
            async { panic!("expired creation must never start") },
            async { Ok(()) },
        )
        .await
        .expect("deadline checkpoint");
        assert!(result.is_none());
    }

    async fn admitted(
        store: &Arc<dyn UserAppLifecycleStore>,
        app_id: &str,
    ) -> UserAppOperationRecord {
        admitted_kind(store, app_id, UserAppOperationKind::EnsureBuilder).await
    }

    async fn admitted_kind(
        store: &Arc<dyn UserAppLifecycleStore>,
        app_id: &str,
        kind: UserAppOperationKind,
    ) -> UserAppOperationRecord {
        store.ensure_identity(app_id).await.expect("identity");
        match store
            .admit(&UserAppAdmission {
                runtime_policy_on_success: None,
                command: None,
                metadata: None,
                app_id: app_id.into(),
                lifecycle_id: None,
                operation_id: uuid::Uuid::new_v4().to_string(),
                request_id: None,
                request_fingerprint: "a".repeat(64),
                kind,
            })
            .await
            .expect("admit")
        {
            UserAppAdmissionOutcome::Accepted(record) => record,
            _ => panic!("fresh application must admit once"),
        }
    }

    #[tokio::test]
    async fn cancelled_creation_marker_requires_failed_dev_ensure_evidence() {
        let directory = tempfile::tempdir().expect("directory");
        let store: Arc<dyn UserAppLifecycleStore> = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &directory.path().join("userapp.turso.db"),
            )
            .await
            .expect("store"),
        );
        for (app_id, kind, state, flag, marked) in [
            (
                "cancelled",
                UserAppOperationKind::EnsureBuilder,
                UserAppOperationState::Failed,
                serde_json::json!(true),
                true,
            ),
            (
                "otherfamily",
                UserAppOperationKind::Start,
                UserAppOperationState::Failed,
                serde_json::json!(true),
                false,
            ),
            (
                "badflag",
                UserAppOperationKind::EnsureBuilder,
                UserAppOperationState::Failed,
                serde_json::json!("true"),
                false,
            ),
            (
                "uncertain",
                UserAppOperationKind::EnsureBuilder,
                UserAppOperationState::RecoveryRequired,
                serde_json::json!(true),
                false,
            ),
        ] {
            let accepted = admitted_kind(&store, app_id, kind).await;
            let running = progress(
                &store,
                &accepted,
                "worker",
                UserAppOperationState::Running,
                "claimed",
                serde_json::Value::Null,
                None,
            )
            .await
            .expect("claim");
            progress(
                &store,
                &running,
                "worker",
                state,
                "creation_result",
                serde_json::json!({"creation_cancelled": flag}),
                Some("interrupted".into()),
            )
            .await
            .expect("persist outcome");
            let budget = if state == UserAppOperationState::RecoveryRequired {
                Duration::from_millis(150)
            } else {
                Duration::from_secs(5)
            };
            let error = wait_record(&store, &accepted, Instant::now() + budget)
                .await
                .expect_err("creation not successful")
                .context("forwarding lookup");
            let marker = error.chain().find_map(|cause| {
                cause.downcast_ref::<crate::userapp_builder::BuilderEnsureSuperseded>()
            });
            assert_eq!(marker.is_some(), marked, "{app_id}: {error:#}");
            if let Some(marker) = marker {
                assert_eq!(marker.operation_id, accepted.operation_id);
            }
            if state == UserAppOperationState::RecoveryRequired {
                let timeout = error
                    .downcast_ref::<shared_types::UserAppWaitTimeout>()
                    .expect("observe the original operation until the caller's deadline");
                assert_eq!(
                    timeout.operation_id.as_deref(),
                    Some(accepted.operation_id.as_str())
                );
                assert_eq!(
                    store
                        .get_operation(app_id, &accepted.operation_id)
                        .await
                        .expect("read")
                        .expect("operation")
                        .state,
                    state
                );
            }
        }
    }
    #[tokio::test]
    async fn final_creation_evidence_survives_terminal_failure_and_rejects_wrong_identity() {
        let directory = tempfile::tempdir().expect("directory");
        let store: Arc<dyn UserAppLifecycleStore> = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &directory.path().join("userapp.turso.db"),
            )
            .await
            .expect("store"),
        );
        let admitted = admitted(&store, "completion").await;
        let running = progress(
            &store,
            &admitted,
            "worker",
            UserAppOperationState::Running,
            "claimed",
            serde_json::Value::Null,
            None,
        )
        .await
        .expect("claim");
        assert!(
            store.reserve_completed_operation(&running).await.is_err(),
            "unknown creation cannot recover"
        );
        let info = ContainerBasicInfo {
            container_id: "physical".into(),
            container_name: "builder".into(),
            container_ip: "127.0.0.1".into(),
            internal_port: 60000,
            external_port: 60000,
            project_id: "completion".into(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: "http://127.0.0.1:60000".into(),
            workload_uid: None,
        };
        let evidence = shared_types::BuilderCreationEvidence {
            creation_lease_released: true,
            target: shared_types::BuilderControlTarget {
                resource_binding: None,
                pod: None,
                restart_image: None,
                workload: Some(shared_types::AppResourceIdentity {
                    kind: shared_types::AppResourceKind::Container,
                    name: "builder".into(),
                    uid: "physical".into(),
                    resource_version: None,
                }),
                context: shared_types::UserAppExecutionContext {
                    app_id: running.app_id.clone(),
                    lifecycle_id: running.lifecycle_id.clone(),
                    operation_id: running.operation_id.clone(),
                    executor_id: "worker".into(),
                    request_fingerprint: running.request_fingerprint.clone(),
                },
            },
            container: info,
        };
        assert!(
            validate_completed_resource(
                &evidence,
                &evidence.target,
                Some(evidence.container.clone())
            )
            .is_ok()
        );
        assert!(validate_completed_resource(&evidence, &evidence.target, None).is_err());
        let mut replacement = evidence.target.clone();
        replacement.workload.as_mut().expect("workload").uid = "replacement".into();
        assert!(
            validate_completed_resource(&evidence, &replacement, Some(evidence.container.clone()))
                .is_err()
        );
        let mut other_pod = evidence.container.clone();
        other_pod.container_id = "other-pod".into();
        assert!(validate_completed_resource(&evidence, &evidence.target, Some(other_pod)).is_err());
        let saved = progress(
            &store,
            &running,
            "worker",
            UserAppOperationState::Running,
            "builder_ready_confirmed",
            serde_json::to_value(&evidence).expect("encode"),
            None,
        )
        .await
        .expect("checkpoint");
        assert!(shared_types::userapp_operation_has_final_evidence(&saved));
        for field in ["operation", "lifecycle", "executor", "physical", "lease"] {
            let mut invalid = evidence.clone();
            match field {
                "operation" => invalid.target.context.operation_id = "other".into(),
                "lifecycle" => invalid.target.context.lifecycle_id = "other".into(),
                "executor" => invalid.target.context.executor_id = "other".into(),
                "physical" => invalid.container.container_id = "replacement".into(),
                _ => invalid.creation_lease_released = false,
            }
            let mut bad = saved.clone();
            bad.checkpoint = serde_json::to_value(invalid).expect("encode");
            assert!(
                !shared_types::userapp_operation_has_final_evidence(&bad),
                "{field}"
            );
        }
        assert!(
            progress(
                &store,
                &running,
                "worker",
                UserAppOperationState::Succeeded,
                "creation_result",
                serde_json::Value::Null,
                None
            )
            .await
            .is_err(),
            "stale terminal CAS must fail"
        );
        let interrupted = progress(
            &store,
            &saved,
            "worker",
            UserAppOperationState::RecoveryRequired,
            &saved.step,
            saved.checkpoint.clone(),
            Some("terminal write interrupted".into()),
        )
        .await
        .expect("interrupt");
        let reserved = store
            .reserve_completed_operation(&interrupted)
            .await
            .expect("reserve released creation without lease receipt");
        assert!(
            store
                .reserve_completed_operation(&interrupted)
                .await
                .is_err(),
            "only one finalizer reserves the snapshot"
        );
        progress(
            &store,
            &reserved,
            "worker",
            UserAppOperationState::Succeeded,
            "creation_result",
            serde_json::to_value(evidence.container).expect("encode"),
            None,
        )
        .await
        .expect("SQL-only finalization");
    }
    #[tokio::test]
    async fn late_subscriber_reads_committed_completion_even_after_signal_removal() {
        let directory = tempfile::tempdir().expect("directory");
        let store: Arc<dyn UserAppLifecycleStore> = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &directory.path().join("userapp.turso.db"),
            )
            .await
            .expect("store"),
        );
        let accepted = admitted(&store, "late").await;
        let running = progress(
            &store,
            &accepted,
            "executor",
            UserAppOperationState::Running,
            "claimed",
            serde_json::Value::Null,
            None,
        )
        .await
        .expect("claim");
        progress(
            &store,
            &running,
            "executor",
            UserAppOperationState::Succeeded,
            "completed",
            serde_json::json!({"container_id":"physical-id"}),
            None,
        )
        .await
        .expect("complete");
        let result = wait_record(&store, &accepted, Instant::now() + Duration::from_secs(1))
            .await
            .expect("late subscriber");
        assert_eq!(result.checkpoint["container_id"], "physical-id");
    }
    #[tokio::test]
    async fn timeout_does_not_cancel_or_resubmit_accepted_operation() {
        let directory = tempfile::tempdir().expect("directory");
        let store: Arc<dyn UserAppLifecycleStore> = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &directory.path().join("userapp.turso.db"),
            )
            .await
            .expect("store"),
        );
        let accepted = admitted(&store, "timeout").await;
        let running = progress(
            &store,
            &accepted,
            "executor",
            UserAppOperationState::Running,
            "claimed",
            serde_json::Value::Null,
            None,
        )
        .await
        .expect("claim");
        let error = wait_record(&store, &accepted, Instant::now())
            .await
            .expect_err("deadline");
        assert_eq!(
            error
                .downcast_ref::<shared_types::UserAppWaitTimeout>()
                .expect("typed wait timeout")
                .operation_id
                .as_deref(),
            Some(accepted.operation_id.as_str())
        );
        let unchanged = store
            .get_operation("timeout", &accepted.operation_id)
            .await
            .expect("read")
            .expect("operation");
        assert_eq!(unchanged.revision, running.revision);
        assert_eq!(unchanged.state, UserAppOperationState::Running);
        progress(
            &store,
            &running,
            "executor",
            UserAppOperationState::Succeeded,
            "completed",
            serde_json::Value::Null,
            None,
        )
        .await
        .expect("worker still completes");
        wait_record(&store, &accepted, Instant::now() + Duration::from_secs(1))
            .await
            .expect("subsequent waiter sees result");
    }
}
