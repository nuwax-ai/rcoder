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
    let signal = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.control_rx.lock().await.recv(),
    )
    .await
    .unwrap()
    .unwrap();
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
async fn source_recovery_keeps_missing_credentials_and_pending_migrations_explicit() {
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
    std::fs::write(
        receipts.join("pending.json"),
        r#"{"identity":"pending","completed":false}"#,
    )
    .unwrap();
    let pg = shared_types::StartPgCredential {
        username: "dev".into(),
        password: "current-credential".into(),
    };
    let error = state
        .credential_recovery(Some(&pg), CredentialRecoveryPurpose::StartCurrentSource)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Migration outcome is unconfirmed")
    );
    assert!(state.runtime_recovery_hold_active());
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
async fn source_recovery_checks_legacy_artifact_migrations_even_without_run_directory() {
    let (_dir, args, state) = fixture(ExecutionTarget::ProjectRun).await;
    assert!(!args.workspace.join(".run").exists());
    let receipts = args.workspace.join("migration-receipts");
    std::fs::create_dir(&receipts).unwrap();
    std::fs::write(
        receipts.join("pending.json"),
        r#"{"identity":"old-artifact-sql","completed":false}"#,
    )
    .unwrap();
    let pg = shared_types::StartPgCredential {
        username: "dev".into(),
        password: "current-credential".into(),
    };
    let error = state
        .credential_recovery(Some(&pg), CredentialRecoveryPurpose::StartCurrentSource)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Migration outcome is unconfirmed")
    );
    assert!(state.runtime_recovery_hold_active());
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
    let signal = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.control_rx.lock().await.recv(),
    )
    .await
    .unwrap()
    .unwrap();
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
