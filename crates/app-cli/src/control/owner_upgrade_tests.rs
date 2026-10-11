//! Real native control/owner-lock tests with an HTTP protocol fixture. These
//! prove upgrade handover, not a real Pingap or business deployment.
use super::*;
use std::{path::PathBuf, sync::Arc, time::Duration};

fn envelope<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::json!({"code":"OK", "message":"ok", "success":true, "data":value})
}

struct OldOwner {
    _root: tempfile::TempDir,
    source: PathBuf,
    state: PathBuf,
    address: String,
    identity: RuntimeIdentityView,
    native: tokio::task::JoinHandle<anyhow::Result<i32>>,
    http: tokio::task::JoinHandle<()>,
}

impl OldOwner {
    async fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        let source = base.join("source");
        let state = base.join("state");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        // Damaged discovery starts the native owner in management-only mode;
        // its live lock and new control identity are authoritative.
        std::fs::write(state.join("supervisor.json"), b"damaged old discovery").unwrap();
        let native_owner = runtime_supervisor::Owner::try_acquire(&state)
            .unwrap()
            .unwrap();
        let session = Arc::new(
            runtime_supervisor::OwnerSession::start(
                native_owner,
                runtime_supervisor::SessionOptions {
                    binding: runtime_supervisor::Binding {
                        component: "app-cli".into(),
                        resource: source.clone(),
                    },
                    policy: runtime_supervisor::Policy::default(),
                    cleanup_adapter: None,
                    restart_on_exit: false,
                    shutdown: tokio_util::sync::CancellationToken::new(),
                },
            )
            .await
            .unwrap(),
        );
        let native = tokio::spawn(session.run(Box::new(|_| {
            anyhow::bail!("an owner protocol upgrade must not launch fixture business")
        })));
        let identity = RuntimeIdentityView {
            application_id: "upgrade-fixture".into(),
            service_family: "userapp-dev".into(),
            workspace_id: "upgrade-workspace".into(),
            source_root: source.display().to_string(),
            runtime_instance_id: uuid::Uuid::new_v4().to_string(),
            deployment_generation_id: "original-deployment".into(),
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: vec!["operations".into()],
        };
        std::fs::write(
            state.join("identity.json"),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        std::fs::write(state.join("token"), b"fixture-upgrade-token\n").unwrap();
        std::fs::write(source.join("sentinel"), b"preserve current source").unwrap();
        let wire_identity = identity.clone();
        let instance = identity.runtime_instance_id.clone();
        let router = axum::Router::new()
            .route(
                "/v1/runtime/identity",
                axum::routing::get(move || {
                    let value = wire_identity.clone();
                    async move { axum::Json(envelope(&value)) }
                }),
            )
            .route(
                "/v1/runtime/status",
                axum::routing::get(move |headers: axum::http::HeaderMap| {
                    let instance = instance.clone();
                    async move {
                        if headers
                            .get("X-Deploy-Token")
                            .and_then(|header| header.to_str().ok())
                            != Some("fixture-upgrade-token")
                        {
                            return (
                                axum::http::StatusCode::UNAUTHORIZED,
                                axum::Json(serde_json::json!({"success":false})),
                            );
                        }
                        let status = shared_types::RuntimeStatusView {
                            desired: shared_types::DesiredState::Stopped,
                            observed: shared_types::ObservedHealth::Stopped,
                            active_target: None,
                            revision: 0,
                            active_operation_id: None,
                            recovery_protection: false,
                            runtime_instance_id: instance,
                        };
                        (axum::http::StatusCode::OK, axum::Json(envelope(&status)))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let http = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            _root: root,
            source,
            state,
            address,
            identity,
            native,
            http,
        }
    }

    async fn assert_released(self) {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), self.native)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            0
        );
        let receipt = runtime_supervisor::last_snapshot(&self.state).unwrap();
        assert_eq!(receipt.phase, runtime_supervisor::Phase::Stopped);
        assert_eq!(receipt.intent, runtime_supervisor::Intent::Shutdown);
        assert!(
            runtime_supervisor::completed_shutdown_for(&self.state, &receipt.supervisor_id)
                .unwrap()
                .is_some()
        );
        assert!(
            runtime_supervisor::Owner::try_acquire(&self.state)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            std::fs::read(self.source.join("sentinel")).unwrap(),
            b"preserve current source"
        );
        self.http.abort();
    }

    async fn close(self) {
        let before = runtime_supervisor::control(
            &self.state,
            runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
        )
        .await
        .unwrap();
        let mut request = runtime_supervisor::Request::new(runtime_supervisor::Action::Shutdown);
        request.capture_generation(before.generation.as_deref());
        runtime_supervisor::shutdown_captured_owner(
            &self.state,
            &before,
            &request,
            Duration::from_secs(3),
        )
        .await
        .unwrap();
        self.assert_released().await;
    }
}

#[tokio::test]
async fn control_only_upgrades_exact_old_owner_and_returns_bootstrap_only_after_release() {
    let owner = OldOwner::start().await;
    let result = reuse_management_owner(
        &owner.address,
        &owner.source,
        &owner.state,
        &owner.identity.application_id,
    )
    .await
    .unwrap();
    assert_eq!(result, ManagementReuse::NoOwner);
    owner.assert_released().await;
}

#[tokio::test]
async fn source_dispatch_upgrades_old_owner_without_submitting_business() {
    let owner = OldOwner::start().await;
    // Invalid source is irrelevant to management upgrading. A new owner will
    // validate the actual current input when the original request is resumed.
    std::fs::write(owner.source.join("workspace.manifest.toml"), b"broken = [").unwrap();
    let result = dispatch_to_owner(
        &owner.address,
        &owner.source,
        &owner.state,
        &owner.identity.application_id,
    )
    .await
    .unwrap();
    assert!(matches!(result, OwnerDispatch::NoOwner));
    assert_eq!(
        std::fs::read(owner.source.join("workspace.manifest.toml")).unwrap(),
        b"broken = ["
    );
    owner.assert_released().await;
}

#[tokio::test]
async fn artifact_dispatch_returns_bootstrap_handover_without_accepting_a_new_deployment() {
    let owner = OldOwner::start().await;
    let args = crate::RuntimeArgs {
        workspace: owner.source.clone(),
        log_dir: owner.state.join("logs"),
        admin_addr: owner.address.clone(),
        pingap_bin: owner.source.join("must-not-run"),
        ..Default::default()
    };
    let deployment = EnvironmentDeployment {
        request: crate::server::DeployRequest {
            runtime_operation_id: None,
            url: "https://example.invalid/original.zip".into(),
            release_id: "original-release".into(),
            sha256: None,
            local_path: None,
            execution_target: None,
            run_pg: None,
        },
        operation_id: uuid::Uuid::new_v4().to_string(),
        generation: "original-generation".into(),
    };
    let journal = owner.state.join("original-deployment-result.json");
    std::fs::write(&journal, b"unconfirmed migration must not be replayed").unwrap();
    let result = dispatch_environment_deployment(
        &args,
        &owner.state,
        &owner.identity.application_id,
        &deployment,
        tokio::time::Instant::now() + Duration::from_secs(3),
    )
    .await
    .unwrap();
    assert_eq!(result, ManagementReuse::NoOwner);
    assert_eq!(
        std::fs::read(journal).unwrap(),
        b"unconfirmed migration must not be replayed"
    );
    owner.assert_released().await;
}

#[tokio::test]
async fn missing_upgrade_capability_does_not_authorize_foreign_or_unauthenticated_shutdown() {
    let owner = OldOwner::start().await;
    let before = runtime_supervisor::last_snapshot(&owner.state).unwrap();
    assert!(
        reuse_management_owner(&owner.address, &owner.source, &owner.state, "another-app")
            .await
            .is_err()
    );
    std::fs::write(owner.state.join("token"), b"wrong-token").unwrap();
    assert!(
        reuse_management_owner(
            &owner.address,
            &owner.source,
            &owner.state,
            &owner.identity.application_id
        )
        .await
        .is_err()
    );
    let after = runtime_supervisor::last_snapshot(&owner.state).unwrap();
    assert_eq!(after.supervisor_id, before.supervisor_id);
    assert_ne!(after.intent, runtime_supervisor::Intent::Shutdown);
    assert!(
        runtime_supervisor::Owner::try_acquire(&owner.state)
            .unwrap()
            .is_none()
    );
    owner.close().await;
}

#[tokio::test]
async fn changed_saved_instance_is_not_retired_based_on_old_capability_probe() {
    let owner = OldOwner::start().await;
    let changed = RuntimeIdentityView {
        runtime_instance_id: uuid::Uuid::new_v4().to_string(),
        ..owner.identity.clone()
    };
    std::fs::write(
        owner.state.join("identity.json"),
        serde_json::to_vec(&changed).unwrap(),
    )
    .unwrap();
    let error = reuse_management_owner(
        &owner.address,
        &owner.source,
        &owner.state,
        &owner.identity.application_id,
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("does not own the observed management API identity"));
    assert!(
        runtime_supervisor::Owner::try_acquire(&owner.state)
            .unwrap()
            .is_none()
    );
    let observed = runtime_supervisor::last_snapshot(&owner.state).unwrap();
    assert_ne!(observed.intent, runtime_supervisor::Intent::Shutdown);
    owner.close().await;
}

#[tokio::test]
async fn upgrade_refuses_resident_evidence_from_another_physical_domain() {
    let owner = OldOwner::start().await;
    let before = runtime_supervisor::last_snapshot(&owner.state).unwrap();
    let mut domain = runtime_supervisor::domain::PhysicalDomain::from_env()
        .unwrap()
        .unwrap_or(runtime_supervisor::domain::PhysicalDomain {
            authority: "container".into(),
            instance_source_env: None,
            instance: "fixture-current".into(),
            volume: "fixture-volume".into(),
        });
    domain.instance = uuid::Uuid::new_v4().to_string();
    let resident = owner.state.join("resident").join(&before.supervisor_id);
    std::fs::create_dir_all(&resident).unwrap();
    let identity = process_utils::command_authority::ResidentIdentity {
        application_id: owner.identity.application_id.clone(),
        binding: serde_json::to_value(&before.binding).unwrap(),
        owner_instance: before.supervisor_id.clone(),
        physical_domain: Some(serde_json::to_value(domain).unwrap()),
        process_epoch: None,
    };
    std::fs::write(
        resident.join("resident-scope.json"),
        serde_json::to_vec(
            &serde_json::json!({"version":1,"identity":identity,"phase":"Running","error":null}),
        )
        .unwrap(),
    )
    .unwrap();
    let error = reuse_management_owner(
        &owner.address,
        &owner.source,
        &owner.state,
        &owner.identity.application_id,
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("physical domain"));
    assert_ne!(
        runtime_supervisor::last_snapshot(&owner.state)
            .unwrap()
            .intent,
        runtime_supervisor::Intent::Shutdown
    );
    assert!(
        runtime_supervisor::Owner::try_acquire(&owner.state)
            .unwrap()
            .is_none()
    );
    owner.close().await;
}
