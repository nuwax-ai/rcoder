use super::*;
use axum::{body::Body, http::Request};
use tower::ServiceExt as _;

fn release(id: &str) -> ReleaseLock {
    let mut lock: ReleaseLock = toml::from_str(
        "schema_version=1\nrelease_id='initial'\nworkspace_name='source-recovery'\nminimum_app_cli_version='0.1.3'\nruntime_image_digest='registry.example/runtime:fixture'\n[pingap]\nmode='managed'\nversion='0.14.3'\ncommit='fixture'\n[[services]]\nservice_id='web'\nname='Web'\ndir='web'\ntype='python'\nkind='web'\nenabled=true\nport=4200\nlogs=[]\n[services.run]\ncommand=['python3','main.py']\nmigrate=[]\ndepends_on=[]\nshutdown_timeout_seconds=3\n[services.health]\nreadiness_path='/ready'\n[services.env]\n",
    ).unwrap();
    lock.release_id = id.into();
    lock
}

async fn fixture(target: ExecutionTarget) -> (tempfile::TempDir, RuntimeArgs, Arc<ServerState>) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(workspace.join("web")).unwrap();
    std::fs::write(
        workspace.join("workspace.manifest.toml"),
        "schema_version=1\n[workspace]\nname='source-recovery'\n",
    )
    .unwrap();
    // A fresh Source request derives from current project inputs, independently
    // of the old active artifact and its historical release lock below.
    std::fs::write(
        workspace.join("web/project.manifest.toml"),
        "schema_version=1\n[project]\nservice_id='web'\nname='Web'\ntype='python'\n[build]\ncommand=['true']\nartifact='source.zip'\n[run]\ncommand=['python3','main.py']\nshutdown_timeout_seconds=3\n[health]\nreadiness_path='/ready'\n",
    )
    .unwrap();
    std::fs::write(
        workspace.join("release.lock.toml"),
        toml::to_string(&release("new-source")).unwrap(),
    )
    .unwrap();
    let args = RuntimeArgs {
        workspace: workspace.clone(),
        ..Default::default()
    };
    let state = Arc::new(ServerState::new(RuntimeStatusService::default()));
    state.initialize_owner_token().unwrap();
    state.set_generation("deployment".into());
    let root =
        crate::runtime_kernel::RuntimeStore::resolve_root(&workspace, "unknown-app").unwrap();
    let mut journal = Journal::open_with_root(&workspace, root).unwrap();
    let request = DeployRequest {
        runtime_operation_id: None,
        url: "source://fixture".into(),
        release_id: "old-request".into(),
        sha256: None,
        local_path: None,
        execution_target: Some(target),
        run_pg: Some(shared_types::StartPgCredential {
            username: "dev".into(),
            password: String::new(),
        }),
    };
    journal
        .write(Receipt {
            generation: "deployment".into(),
            boundary: Boundary::Active,
            operation: shared_types::AppDeploymentOperation {
                operation_id: "old-operation".into(),
                deployment_generation_id: "deployment".into(),
                deploy_stage: AppDeploymentStage::Succeeded,
                persisted: true,
                request_release_id: "old-request".into(),
                artifact_release_id: Some("old-artifact".into()),
                recovery: None,
                phase: AppCliDeployPhase::Running,
                error: None,
            },
            request: request.clone(),
            active: Some(ActiveVersion {
                artifact_release_id: "old-artifact".into(),
                request: Some(request),
            }),
        })
        .unwrap();
    *state.journal.lock().unwrap() = Some(journal);
    let kernel = assemble_runtime_kernel(&state, &args).await.unwrap();
    state.set_runtime_kernel(kernel);
    state.begin_credentials_hold();
    state.mark_initialized();
    (dir, args, state)
}

async fn source_signal(state: &ServerState, operation_id: &str) -> ControlSignal {
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.control_rx.lock().await.recv(),
    )
    .await
    {
        Ok(Some(signal)) => signal,
        outcome => panic!(
            "Source dispatch {operation_id} missing ({outcome:?}); original operation: {:?}",
            state
                .runtime_kernel()
                .unwrap()
                .get(operation_id)
                .await
                .unwrap()
        ),
    }
}

#[tokio::test]
async fn fresh_owner_restores_actual_persisted_pg_into_business_input() {
    let (_dir, args, old) = fixture(ExecutionTarget::Source).await;
    let pg = shared_types::StartPgCredential {
        username: "business".into(),
        password: "owner-restart-private-credential".into(),
    };
    {
        let mut guard = old.journal.lock().unwrap();
        let journal = guard.as_mut().unwrap();
        let mut receipt = journal.receipt.clone().unwrap();
        receipt.request.run_pg = Some(pg.clone());
        receipt.active.as_mut().unwrap().artifact_release_id = "new-source".into();
        receipt.operation.artifact_release_id = Some("new-source".into());
        receipt
            .active
            .as_mut()
            .unwrap()
            .request
            .as_mut()
            .unwrap()
            .run_pg = Some(pg.clone());
        journal.write(receipt).unwrap();
        // Simulate the old owner's lease ending; the new owner reads the file.
        drop(guard.take());
    }
    let fresh = Arc::new(ServerState::new(RuntimeStatusService::default()));
    let kernel = assemble_runtime_kernel(&fresh, &args).await.unwrap();
    fresh.set_runtime_kernel(kernel);
    let restored = restored_runtime_args(&args, &fresh).unwrap();
    // macOS 的 tempfile 根经 /var → /private/var 符号链接，恢复侧返回规范化
    // 拼写——断言按规范化形态比较（Linux 上 canonicalize 恒等，语义不变）。
    let expected_workspace = args.workspace.canonicalize().unwrap();
    assert_eq!(restored.workspace, expected_workspace);
    assert!(
        matches!(initialize_startup(&args, &fresh).await.unwrap(), Some(InitialAction::Existing { workspace }) if workspace == expected_workspace)
    );
    assert_eq!(fresh.take_pending_run_config(), Some(pg));
    assert!(!fresh.runtime_recovery_hold_active());
}

#[tokio::test]
async fn legacy_redacted_receipt_is_diagnostic_and_does_not_hold_no_pg_business() {
    let (_dir, args, state) = fixture(ExecutionTarget::Source).await;
    state
        .runtime_recovery_hold
        .store(0, std::sync::atomic::Ordering::Release);
    {
        let mut guard = state.journal.lock().unwrap();
        let journal = guard.as_mut().unwrap();
        let mut receipt = journal.receipt.clone().unwrap();
        receipt.active.as_mut().unwrap().artifact_release_id = "new-source".into();
        receipt.operation.artifact_release_id = Some("new-source".into());
        journal.write(receipt).unwrap();
    }
    let root =
        crate::runtime_kernel::RuntimeStore::resolve_root(&args.workspace, "unknown-app").unwrap();
    let original = std::fs::read(root.join(".deploy-operation.json")).unwrap();
    let restored = restored_runtime_args(&args, &state).unwrap();
    let expected_workspace = args.workspace.canonicalize().unwrap();
    assert_eq!(restored.workspace, expected_workspace);
    assert!(
        matches!(initialize_startup(&args, &state).await.unwrap(), Some(InitialAction::Existing { workspace }) if workspace == expected_workspace)
    );
    assert!(
        !state.runtime_recovery_hold_active(),
        "legacy redaction must not create a permanent credentials hold"
    );
    assert!(!state.recovery_view().await.unwrap().credentials_required);
    if let Some(pg) = state.take_pending_run_config() {
        assert!(
            !pg.password.is_empty(),
            "redacted history is not an actual empty credential"
        );
    }
    assert_eq!(
        std::fs::read(root.join(".deploy-operation.json")).unwrap(),
        original,
        "recovery must not rewrite historical redaction into invented credentials"
    );
}

#[tokio::test]
async fn current_explicit_input_wins_over_historical_redacted_credentials() {
    let (_dir, args, state) = fixture(ExecutionTarget::Source).await;
    state
        .runtime_recovery_hold
        .store(0, std::sync::atomic::Ordering::Release);
    let current = shared_types::StartPgCredential {
        username: "current_business".into(),
        password: "new-current-private-credential".into(),
    };
    state.set_pending_run_config(Some(current.clone()));
    restored_runtime_args(&args, &state).unwrap();
    assert_eq!(state.take_pending_run_config(), Some(current));
    assert!(!state.runtime_recovery_hold_active());
}

#[tokio::test]
async fn new_explicit_empty_pg_is_rejected_without_a_legacy_credentials_hold() {
    let (_dir, _args, state) = fixture(ExecutionTarget::Source).await;
    state
        .runtime_recovery_hold
        .store(0, std::sync::atomic::Ordering::Release);
    let invalid = shared_types::StartPgCredential {
        username: "business".into(),
        password: String::new(),
    };
    let error = state
        .credential_recovery(
            Some(&invalid),
            CredentialRecoveryPurpose::StartCurrentSource,
        )
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<shared_types::SourceRunCredentialError>(),
        Some(&shared_types::SourceRunCredentialError::InvalidPassword)
    );
    assert!(!state.runtime_recovery_hold_active());
}

#[tokio::test]
async fn internal_pg_input_is_absent_from_http_recovery_and_operation_sse_views() {
    let (_dir, args, state) = fixture(ExecutionTarget::Source).await;
    let kernel = state.runtime_kernel().unwrap();
    let identity = kernel.identity().clone();
    let request = serde_json::json!({
        "operation_id": "privateinputviews",
        "expected_runtime_instance_id": identity.runtime_instance_id,
        "expected_revision": kernel.status().await.unwrap().revision,
        "workspace_id": identity.workspace_id,
        "kind": "start",
        "profile": {"profile": "source", "input": {"workspace_id": identity.workspace_id}},
        "run_config": {"pg": {"username": "business", "password": "api-private-credential"}}
    });
    let router = crate::api::bound_router(
        args.workspace.clone(),
        args.log_dir,
        args.pingap_bin,
        state.clone(),
    );
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runtime/operations")
                .header("content-type", "application/json")
                .header("x-deploy-token", state.control_token().unwrap())
                .body(Body::from(serde_json::to_vec(&request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let accepted = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&accepted).contains("api-private-credential"));
    let signal = source_signal(&state, "privateinputviews").await;
    assert!(matches!(
        settle_control_signal(&state, signal).await,
        InitialAction::Source
    ));
    let root =
        crate::runtime_kernel::RuntimeStore::resolve_root(&args.workspace, "unknown-app").unwrap();
    let operation: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("operations/privateinputviews.json")).unwrap(),
    )
    .unwrap();
    let deployment: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join(".deploy-operation.json")).unwrap())
            .unwrap();
    assert_eq!(
        operation["request"]["run_config"]["pg"]["password"],
        "api-private-credential"
    );
    assert_eq!(
        deployment["request"]["run_pg"]["password"],
        "api-private-credential"
    );
    let denied = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/runtime/recovery")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), axum::http::StatusCode::FORBIDDEN);
    let bytes = axum::body::to_bytes(denied.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("api-private-credential"));
    for url in [
        "/v1/runtime/status",
        "/v1/runtime/recovery",
        "/v1/runtime/operations/privateinputviews",
        "/v1/runtime/operations/privateinputviews/events",
        "/v1/runtime/operations/privateinputviews/events/stream",
        "/v1/deploy/status",
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(url)
                    .header("x-deploy-token", state.control_token().unwrap())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{url}: {}",
            response.status()
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(
            !body.contains("api-private-credential"),
            "{url} leaked private input"
        );
        assert!(
            !body.contains("\"run_config\"") && !body.contains("\"run_pg\""),
            "{url} exposed internal request fields"
        );
    }
}

#[tokio::test]
async fn fresh_source_with_new_lock_can_supply_redacted_credentials() {
    let (_dir, _args, state) = fixture(ExecutionTarget::Source).await;
    let pg = shared_types::StartPgCredential {
        username: "dev".into(),
        password: "current-credential".into(),
    };
    assert!(
        state
            .credential_recovery(Some(&pg), CredentialRecoveryPurpose::StartCurrentSource)
            .unwrap()
            .supplies_credentials(),
        "a new Source operation must not need the previous artifact's release ID"
    );
    assert!(
        state.runtime_recovery_hold_active(),
        "eligibility must not clear the hold before admission"
    );
}

#[tokio::test]
async fn fresh_source_can_replace_an_old_artifact_without_its_directory() {
    let (_dir, args, state) = fixture(ExecutionTarget::ProjectRun).await;
    assert!(!args.workspace.join(".run").exists());
    let pg = shared_types::StartPgCredential {
        username: "dev".into(),
        password: "current-credential".into(),
    };
    assert!(
        state
            .credential_recovery(Some(&pg), CredentialRecoveryPurpose::StartCurrentSource)
            .unwrap()
            .supplies_credentials(),
        "the current Source target must not be constrained by a missing previous artifact"
    );
}

#[tokio::test]
async fn source_recovery_router_admits_and_dispatches_the_new_operation() {
    let (_dir, args, state) = fixture(ExecutionTarget::Source).await;
    let kernel = state.runtime_kernel().unwrap();
    let identity = kernel.identity().clone();
    let request = shared_types::RuntimeOperationRequest {
        operation_id: "new-source-request".into(),
        expected_runtime_instance_id: identity.runtime_instance_id,
        expected_revision: kernel.status().await.unwrap().revision,
        workspace_id: identity.workspace_id.clone(),
        kind: shared_types::RuntimeOperationKind::Restart,
        profile: shared_types::RunProfileInput::Source {
            workspace_id: identity.workspace_id,
        },
        run_config: Some(shared_types::OperationRunConfig {
            pg: Some(shared_types::StartPgCredential {
                username: "dev".into(),
                password: "current-credential".into(),
            }),
        }),
        request_context: None,
    };
    let router = crate::api::bound_router(
        args.workspace.clone(),
        args.log_dir,
        args.pingap_bin,
        state.clone(),
    );
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/runtime/operations")
                .header("content-type", "application/json")
                .header("x-deploy-token", state.control_token().unwrap())
                .body(Body::from(serde_json::to_vec(&request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let signal = source_signal(&state, "new-source-request").await;
    let ControlSignal::OrchestrateSource {
        operation_id, pg, ..
    } = signal
    else {
        panic!("Source dispatch expected")
    };
    assert_eq!(operation_id, "new-source-request");
    assert_eq!(pg.unwrap().password, "current-credential");
    assert!(!state.runtime_recovery_hold_active());
}

#[tokio::test]
async fn source_recovery_does_not_need_an_active_history_record() {
    let (_dir, _args, state) = fixture(ExecutionTarget::Source).await;
    state.journal.lock().unwrap().as_mut().unwrap().receipt = None;
    let pg = shared_types::StartPgCredential {
        username: "dev".into(),
        password: "current-credential".into(),
    };
    assert!(
        state
            .credential_recovery(Some(&pg), CredentialRecoveryPurpose::StartCurrentSource)
            .unwrap()
            .supplies_credentials()
    );
    assert!(
        state
            .credential_recovery(
                Some(&pg),
                CredentialRecoveryPurpose::RestoreConfirmedArtifact
            )
            .is_err()
    );
}

#[tokio::test]
async fn missing_legacy_artifact_is_not_an_unknown_data_writer() {
    let (_dir, args, state) = fixture(ExecutionTarget::ProjectRun).await;
    {
        let mut guard = state.journal.lock().unwrap();
        let mut receipt = guard.as_ref().unwrap().receipt.clone().unwrap();
        for request in [
            &mut receipt.request,
            receipt.active.as_mut().unwrap().request.as_mut().unwrap(),
        ] {
            request.execution_target = None;
            request.local_path = Some(args.workspace.join("builds/missing.zip"));
        }
        guard.as_mut().unwrap().write(receipt).unwrap();
    }
    assert!(restored_runtime_args_inner(&args, &state, false).is_err());
    assert!(state.source_replacement_hold_only());
    assert!(
        !state
            .shutdown_unconfirmed
            .load(std::sync::atomic::Ordering::Acquire)
    );
    let pg = shared_types::StartPgCredential {
        username: "dev".into(),
        password: "current-credential".into(),
    };
    assert!(
        state
            .credential_recovery(Some(&pg), CredentialRecoveryPurpose::StartCurrentSource)
            .unwrap()
            .supplies_credentials()
    );
}

#[tokio::test]
async fn recovery_handoff_preserves_new_unknown_bits_and_ignores_old_callbacks() {
    let (_dir, _args, state) = fixture(ExecutionTarget::Source).await;
    let pg = shared_types::StartPgCredential {
        username: "dev".into(),
        password: "current-credential".into(),
    };
    let evaluated = state
        .credential_recovery(Some(&pg), CredentialRecoveryPurpose::StartCurrentSource)
        .unwrap();
    assert!(evaluated.supplies_credentials());
    state.begin_runtime_recovery_hold();
    assert!(
        state
            .consume_credentials_hold("request-a", evaluated)
            .is_err()
    );
    assert_eq!(
        state
            .runtime_recovery_hold
            .load(std::sync::atomic::Ordering::Acquire),
        CREDENTIALS_HOLD | OUTCOME_HOLD
    );
    state
        .runtime_recovery_hold
        .store(SOURCE_HISTORY_HOLD, std::sync::atomic::Ordering::Release);
    let evaluated = state
        .credential_recovery(None, CredentialRecoveryPurpose::StartCurrentSource)
        .unwrap();
    state
        .consume_credentials_hold("request-b", evaluated)
        .unwrap();
    state.settle_credential_recovery("request-a", false);
    assert!(!state.runtime_recovery_hold_active());
    state.settle_credential_recovery("request-b", false);
    assert_eq!(
        state
            .runtime_recovery_hold
            .load(std::sync::atomic::Ordering::Acquire),
        SOURCE_HISTORY_HOLD
    );
}

#[tokio::test]
async fn source_recovery_requires_current_credentials_while_pending_migrations_are_advisory() {
    let (_dir, args, state) = fixture(ExecutionTarget::Source).await;
    assert!(
        state
            .credential_recovery(None, CredentialRecoveryPurpose::StartCurrentSource)
            .unwrap_err()
            .to_string()
            .contains("database runtime credentials")
    );
    let receipts = args.workspace.parent().unwrap().join("migration-receipts");
    std::fs::create_dir(&receipts).unwrap();
    let pending = br#"{"identity":"pending","completed":false}"#;
    std::fs::write(receipts.join("pending.json"), pending).unwrap();
    let pg = shared_types::StartPgCredential {
        username: "dev".into(),
        password: "current-credential".into(),
    };
    let recovery = state
        .credential_recovery(Some(&pg), CredentialRecoveryPurpose::StartCurrentSource)
        .unwrap();
    assert!(recovery.supplies_credentials());
    assert!(state.runtime_recovery_hold_active());
    assert_eq!(
        std::fs::read(receipts.join("pending.json")).unwrap(),
        pending
    );
    assert!(!crate::migration_journal::inspect_migrations(&args.workspace).unwrap());
}

#[tokio::test]
async fn accepted_stop_blocks_a_source_request_captured_before_it() {
    let (_dir, _args, state) = fixture(ExecutionTarget::Source).await;
    let kernel = state.runtime_kernel().unwrap();
    let identity = kernel.identity();
    let revision = kernel.status().await.unwrap().revision;
    let pg = shared_types::StartPgCredential {
        username: "dev".into(),
        password: "current-credential".into(),
    };
    assert!(
        state
            .credential_recovery(Some(&pg), CredentialRecoveryPurpose::StartCurrentSource)
            .unwrap()
            .supplies_credentials()
    );
    let mut request = shared_types::RuntimeOperationRequest {
        operation_id: "stop-wins".into(),
        expected_runtime_instance_id: identity.runtime_instance_id.clone(),
        expected_revision: revision,
        workspace_id: identity.workspace_id.clone(),
        kind: shared_types::RuntimeOperationKind::Stop,
        profile: shared_types::RunProfileInput::Source {
            workspace_id: identity.workspace_id.clone(),
        },
        run_config: None,
        request_context: None,
    };
    kernel
        .admit_with_owner_hold(request.clone(), true)
        .await
        .unwrap();
    request.operation_id = "late-source".into();
    request.kind = shared_types::RuntimeOperationKind::Restart;
    request.run_config = Some(shared_types::OperationRunConfig { pg: Some(pg) });
    let error = kernel
        .admit_with_owner_hold(request, false)
        .await
        .unwrap_err();
    assert_eq!(error.code, shared_types::ERR_REVISION_MISMATCH);
    assert!(kernel.get("late-source").await.unwrap().is_none());
    assert!(state.runtime_recovery_hold_active());
    assert_eq!(
        kernel.status().await.unwrap().desired,
        shared_types::DesiredState::Stopped
    );
}

#[tokio::test]
async fn source_recovery_accepts_legacy_artifact_migrations_without_run_directory() {
    let (_dir, args, state) = fixture(ExecutionTarget::ProjectRun).await;
    assert!(!args.workspace.join(".run").exists());
    let receipts = args.workspace.join("migration-receipts");
    std::fs::create_dir(&receipts).unwrap();
    let pending = br#"{"identity":"old-artifact-sql","completed":false}"#;
    std::fs::write(receipts.join("pending.json"), pending).unwrap();
    let pg = shared_types::StartPgCredential {
        username: "dev".into(),
        password: "current-credential".into(),
    };
    let recovery = state
        .credential_recovery(Some(&pg), CredentialRecoveryPurpose::StartCurrentSource)
        .unwrap();
    assert!(recovery.supplies_credentials());
    assert!(state.runtime_recovery_hold_active());
    assert_eq!(
        std::fs::read(receipts.join("pending.json")).unwrap(),
        pending
    );
    assert!(!crate::migration_journal::inspect_migrations(&args.workspace).unwrap());
    assert!(state.control_rx.lock().await.try_recv().is_err());
}

#[tokio::test]
async fn historical_deployment_generation_does_not_veto_a_new_source_request() {
    let (_dir, args, state) = fixture(ExecutionTarget::Source).await;
    {
        let mut journal = state.journal.lock().unwrap();
        let mut receipt = journal.as_ref().unwrap().receipt.clone().unwrap();
        receipt.generation = "old-deployment".into();
        receipt.operation.deployment_generation_id = "old-deployment".into();
        journal.as_mut().unwrap().write(receipt).unwrap();
    }
    assert!(restored_runtime_args_inner(&args, &state, false).is_err());
    assert!(state.source_replacement_hold_only());
    let pg = shared_types::StartPgCredential {
        username: "dev".into(),
        password: "current-credential".into(),
    };
    assert!(
        state
            .credential_recovery(Some(&pg), CredentialRecoveryPurpose::StartCurrentSource)
            .unwrap()
            .supplies_credentials()
    );
    assert!(
        state
            .credential_recovery(
                Some(&pg),
                CredentialRecoveryPurpose::RestoreConfirmedArtifact
            )
            .is_err()
    );
}

#[tokio::test]
async fn inconsistent_history_identity_keeps_management_and_fresh_source_available() {
    let (_dir, args, first) = fixture(ExecutionTarget::Source).await;
    let root =
        crate::runtime_kernel::RuntimeStore::resolve_root(&args.workspace, "unknown-app").unwrap();
    {
        let mut slot = first.journal.lock().unwrap();
        let mut journal = slot.take().unwrap();
        let mut receipt = journal.receipt.clone().unwrap();
        receipt.operation.deployment_generation_id = "inconsistent-legacy-value".into();
        journal.write(receipt).unwrap();
    }
    let original = std::fs::read(root.join(".deploy-operation.json")).unwrap();
    let restarted = Arc::new(ServerState::new(RuntimeStatusService::default()));
    restarted.initialize_owner_token().unwrap();
    let kernel = assemble_runtime_kernel(&restarted, &args).await.unwrap();
    assert!(restarted.set_runtime_kernel(kernel));
    assert!(
        restarted.source_replacement_hold_only(),
        "inconsistent ordinary history must not become an unknown writer"
    );
    assert_eq!(
        std::fs::read(root.join(".deploy-operation.json")).unwrap(),
        original
    );
    let journal = restarted.journal.lock().unwrap();
    let saved = journal.as_ref().unwrap().preserve_source_history().unwrap();
    assert_eq!(std::fs::read(saved).unwrap(), original);
    assert_eq!(
        std::fs::read_dir(&root)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".deploy-operation.source-history-")
            })
            .count(),
        1,
        "retry must reuse the same original backup"
    );
    drop(journal);
    let pg = shared_types::StartPgCredential {
        username: "dev".into(),
        password: "current-credential".into(),
    };
    assert!(
        restarted
            .credential_recovery(Some(&pg), CredentialRecoveryPurpose::StartCurrentSource)
            .unwrap()
            .supplies_credentials()
    );
}

#[tokio::test]
async fn recovery_handoff_cannot_consume_credentials_required_after_eligibility() {
    let (_dir, _args, state) = fixture(ExecutionTarget::Source).await;
    state
        .runtime_recovery_hold
        .store(SOURCE_HISTORY_HOLD, std::sync::atomic::Ordering::Release);
    let evaluated = state
        .credential_recovery(None, CredentialRecoveryPurpose::StartCurrentSource)
        .unwrap();
    assert!(evaluated.supplies_credentials());
    state.begin_credentials_hold();
    assert!(
        state
            .consume_credentials_hold("no-private-input", evaluated)
            .is_err(),
        "handoff must not clear a credential requirement that was never validated"
    );
    assert_eq!(
        state
            .runtime_recovery_hold
            .load(std::sync::atomic::Ordering::Acquire),
        SOURCE_HISTORY_HOLD | CREDENTIALS_HOLD
    );
}

#[tokio::test]
async fn rejected_before_dispatch_is_failed_and_a_new_valid_source_can_retry() {
    let (_dir, _args, state) = fixture(ExecutionTarget::Source).await;
    let kernel = state.runtime_kernel().unwrap();
    let identity = kernel.identity();
    state
        .runtime_recovery_hold
        .store(SOURCE_HISTORY_HOLD, std::sync::atomic::Ordering::Release);
    assert!(
        state
            .credential_recovery(None, CredentialRecoveryPurpose::StartCurrentSource)
            .unwrap()
            .supplies_credentials()
    );
    let mut request = shared_types::RuntimeOperationRequest {
        operation_id: "late-credentials-required".into(),
        expected_runtime_instance_id: identity.runtime_instance_id.clone(),
        expected_revision: kernel.status().await.unwrap().revision,
        workspace_id: identity.workspace_id.clone(),
        kind: shared_types::RuntimeOperationKind::Restart,
        profile: shared_types::RunProfileInput::Source {
            workspace_id: identity.workspace_id.clone(),
        },
        run_config: None,
        request_context: None,
    };
    // The prerequisite changes after the API's read-only check but before the
    // kernel dispatch. No command has started; this is a known rejection.
    state.begin_credentials_hold();
    kernel
        .admit_with_owner_hold(request.clone(), false)
        .await
        .unwrap();
    let rejected = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let operation = kernel.get(&request.operation_id).await.unwrap().unwrap();
            if operation.state.is_terminal()
                || operation.state == shared_types::RuntimeOperationState::RecoveryRequired
            {
                break operation;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(rejected.state, shared_types::RuntimeOperationState::Failed);
    assert!(
        rejected
            .error_message
            .as_deref()
            .unwrap()
            .contains("database runtime credentials")
    );
    assert!(!kernel.status().await.unwrap().recovery_protection);
    assert!(state.control_rx.lock().await.try_recv().is_err());
    request.operation_id = "valid-retry".into();
    request.expected_revision = kernel.status().await.unwrap().revision;
    request.run_config = Some(shared_types::OperationRunConfig {
        pg: Some(shared_types::StartPgCredential {
            username: "dev".into(),
            password: "current-credential".into(),
        }),
    });
    kernel.admit_with_owner_hold(request, false).await.unwrap();
    let signal = source_signal(&state, "valid-retry").await;
    let ControlSignal::OrchestrateSource { operation_id, .. } = signal else {
        panic!("Source expected")
    };
    assert_eq!(operation_id, "valid-retry");
}

#[tokio::test]
async fn recovery_api_selects_request_language_and_defaults_to_english() {
    use http_body_util::BodyExt as _;
    let (_dir, args, state) = fixture(ExecutionTarget::Source).await;
    let kernel = state.runtime_kernel().unwrap();
    let identity = kernel.identity();
    let router =
        crate::api::bound_router(args.workspace, args.log_dir, args.pingap_bin, state.clone());
    for (number, language, expected) in [
        (0, None, "database runtime credentials"),
        (1, Some("zh-CN"), "数据库运行凭据"),
        (2, Some("zh-TW"), "資料庫執行憑證"),
        (3, Some("fr-FR"), "database runtime credentials"),
    ] {
        let request = shared_types::RuntimeOperationRequest {
            operation_id: format!("language-{number}"),
            expected_runtime_instance_id: identity.runtime_instance_id.clone(),
            expected_revision: kernel.status().await.unwrap().revision,
            workspace_id: identity.workspace_id.clone(),
            kind: shared_types::RuntimeOperationKind::Restart,
            profile: shared_types::RunProfileInput::Source {
                workspace_id: identity.workspace_id.clone(),
            },
            run_config: None,
            request_context: None,
        };
        let mut builder = Request::builder()
            .method("POST")
            .uri("/v1/runtime/operations")
            .header("content-type", "application/json")
            .header("x-deploy-token", state.control_token().unwrap());
        if let Some(language) = language {
            builder = builder.header("accept-language", language);
        }
        let response = router
            .clone()
            .oneshot(
                builder
                    .body(Body::from(serde_json::to_vec(&request).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: shared_types::HttpResult<serde_json::Value> =
            serde_json::from_slice(&bytes).unwrap();
        assert!(!body.success);
        assert_eq!(body.code, shared_types::ERR_RECOVERY_REQUIRED);
        assert!(body.message.contains(expected), "{}", body.message);
        assert!(
            kernel.get(&request.operation_id).await.unwrap().is_none(),
            "localization must not admit work"
        );
    }
    assert!(state.runtime_recovery_hold_active());
}

#[tokio::test]
async fn advisory_receipts_admit_and_dispatch_changed_source_from_both_legacy_roots() {
    for (target, corrupt) in [
        (ExecutionTarget::Source, false),
        (ExecutionTarget::Source, true),
        (ExecutionTarget::ProjectRun, false),
        (ExecutionTarget::ProjectRun, true),
    ] {
        let (_dir, args, state) = fixture(target).await;
        let previous = if corrupt {
            b"invalid-receipt".as_slice()
        } else {
            br#"{"identity":"old-source-migration","completed":false}"#.as_slice()
        };
        let paths = [
            args.workspace
                .parent()
                .unwrap()
                .join("migration-receipts/old.json"),
            args.workspace.join("migration-receipts/old.json"),
        ];
        for path in &paths {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, previous).unwrap();
        }
        let manifest = args.workspace.join("web/project.manifest.toml");
        let old_manifest = std::fs::read_to_string(&manifest).unwrap();
        std::fs::write(
            &manifest,
            old_manifest.replace("name='Web'", "name='Current Web'"),
        )
        .unwrap();
        let old_release = crate::manifest::read_release_lock(&args.workspace)
            .unwrap()
            .release_id;
        let kernel = state.runtime_kernel().unwrap();
        let identity = kernel.identity().clone();
        let request = shared_types::RuntimeOperationRequest {
            operation_id: "advisory-history-source".into(),
            expected_runtime_instance_id: identity.runtime_instance_id,
            expected_revision: kernel.status().await.unwrap().revision,
            workspace_id: identity.workspace_id.clone(),
            kind: shared_types::RuntimeOperationKind::Restart,
            profile: shared_types::RunProfileInput::Source {
                workspace_id: identity.workspace_id,
            },
            run_config: Some(shared_types::OperationRunConfig {
                pg: Some(shared_types::StartPgCredential {
                    username: "dev".into(),
                    password: "current-credential".into(),
                }),
            }),
            request_context: None,
        };
        let router = crate::api::bound_router(
            args.workspace.clone(),
            args.log_dir,
            args.pingap_bin,
            state.clone(),
        );
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/runtime/operations")
                    .header("content-type", "application/json")
                    .header("x-deploy-token", state.control_token().unwrap())
                    .body(Body::from(serde_json::to_vec(&request).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::ACCEPTED,
            "pending/corrupt receipt must not reject fresh source admission"
        );
        let signal = source_signal(&state, "advisory-history-source").await;
        let ControlSignal::OrchestrateSource {
            operation_id, pg, ..
        } = signal
        else {
            panic!("Source dispatch expected")
        };
        assert_eq!(operation_id, request.operation_id);
        assert_eq!(pg.unwrap().password, "current-credential");
        assert!(kernel.get(&request.operation_id).await.unwrap().is_some());
        assert!(!state.runtime_recovery_hold_active());
        let current = crate::manifest::read_release_lock(&args.workspace).unwrap();
        assert_ne!(current.release_id, old_release);
        assert_eq!(current.services[0].name, "Current Web");
        for path in &paths {
            assert_eq!(std::fs::read(path).unwrap(), previous);
        }
    }
}
