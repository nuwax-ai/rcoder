//! Real PG admission through AppState; runtime proof is a protocol fixture.
use super::*;
use crate::userapp_builder::{FenceRuntime, builder_runtime_test_state as runtime_state};
use axum::{body::to_bytes, http::StatusCode, response::IntoResponse};
use rcoder_storage::{ProjectStoreBackend, config::PostgresConfig, pg::PgStore};
use shared_types::{AgentWorkspaceIdentity, ServiceType, VerifiedAgentReplacement};
use std::{sync::atomic::Ordering, time::Duration};

struct Fixture {
    state: Arc<AppState>,
    local: Arc<PgStore>,
    peer: Arc<PgStore>,
    runtime: Arc<FenceRuntime>,
    project_id: String,
    owner: String,
    old: ContainerBasicInfo,
    observed: ContainerBasicInfo,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let dsn = std::env::var("RCODER_PG_TEST_DSN")
            .ok()
            .filter(|value| !value.is_empty());
        assert!(
            dsn.is_some() || std::env::var("RCODER_PG_TEST_STRICT").as_deref() != Ok("1"),
            "Strict PG tests require an isolated DSN"
        );
        let Some(dsn) = dsn else {
            eprintln!("[skip] RCODER_PG_TEST_DSN missing for engine peer-retirement contract");
            return None;
        };
        let config = PostgresConfig {
            url: Some(dsn),
            ..Default::default()
        };
        let local = Arc::new(
            PgStore::connect(
                &config,
                "engine-retirement-test".into(),
                "cluster.local".into(),
            )
            .await
            .unwrap(),
        );
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let project_id = format!("engine-retirement-{suffix}");
        let owner = format!("engine-owner-{suffix}");
        let old = ContainerBasicInfo {
            container_id: format!("old-pod-{suffix}"),
            container_name: format!("computer-{suffix}"),
            container_ip: "127.0.0.1".into(),
            internal_port: shared_types::GRPC_DEFAULT_PORT,
            external_port: 0,
            project_id: owner.clone(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: "http://127.0.0.1:50051".into(),
            workload_uid: Some(format!("old-sts-{suffix}")),
        };
        let mut registered = ProjectAndContainerInfo::new(project_id.clone());
        registered.set_user_id(Some(owner.clone()));
        registered.set_service_type(Some(ServiceType::ComputerAgentRunner));
        registered.set_container(Some(old.clone()));
        local
            .insert_with_session_durable(
                project_id.clone(),
                Arc::new(registered),
                &format!("retired-session-{suffix}"),
            )
            .await
            .unwrap();
        // A and B are independent PgStores. No sync runs between peer deletion
        // and the local AppState request, preserving the exact stale projection.
        let peer = Arc::new(
            PgStore::connect(
                &config,
                "engine-retirement-peer".into(),
                "cluster.local".into(),
            )
            .await
            .unwrap(),
        );
        let mut observed = old.clone();
        observed.container_id = format!("new-pod-{suffix}");
        observed.workload_uid = Some(format!("new-sts-{suffix}"));
        observed.container_ip = "127.0.0.2".into();
        observed.service_url = "http://127.0.0.2:50051".into();
        let runtime = Arc::new(FenceRuntime::default());
        *runtime.agent_replacement.lock().unwrap() = Some(VerifiedAgentReplacement {
            identifier: owner.clone(),
            service_type: ServiceType::ComputerAgentRunner,
            previous: Some(old.clone()),
            replacement: observed.clone(),
            workspace: AgentWorkspaceIdentity {
                namespace: "engine-retirement-test".into(),
                claim_name: "retained-workspace".into(),
                claim_uid: "retained-workspace-uid".into(),
                mount_path: "/home/user".into(),
                sub_path: None,
            },
        });
        let directory = tempfile::tempdir().unwrap();
        let lifecycle = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &directory.path().join("lifecycle.db"),
            )
            .await
            .unwrap(),
        );
        let mut state = runtime_state(runtime.clone(), lifecycle, None).await;
        Arc::get_mut(&mut state).unwrap().projects =
            Arc::new(ProjectStoreBackend::Postgres(local.clone()));
        Some(Self {
            state,
            local,
            peer,
            runtime,
            project_id,
            owner,
            old,
            observed,
            _directory: directory,
        })
    }

    fn params(&self) -> ContainerCreateParams {
        ContainerCreateParams::builder()
            .project_id(&self.project_id)
            .user_id(&self.owner)
            .service_type(ServiceType::ComputerAgentRunner)
            .build()
    }

    fn incoming(&self, id: &str) -> ProjectAndContainerInfo {
        let mut incoming = ProjectAndContainerInfo::new(id.to_owned());
        incoming.set_user_id(Some(self.owner.clone()));
        incoming.set_service_type(Some(ServiceType::ComputerAgentRunner));
        incoming.set_container(Some(self.observed.clone()));
        incoming.set_request_id(Some("current-request".into()));
        incoming
    }

    async fn delete_from_peer(&self) {
        assert_eq!(
            self.peer
                .delete_container_with_projects_durable(&self.old.container_id)
                .await,
            (true, 1)
        );
        let stale = self.local.get(&self.project_id).unwrap();
        assert_eq!(
            stale.container_info().unwrap().container_id,
            self.old.container_id
        );
        assert!(!stale.sessions().is_empty());
    }

    async fn shutdown(&self) {
        for store in [&self.local, &self.peer] {
            assert!(
                store
                    .shutdown_flush_outcome(Duration::from_secs(5))
                    .await
                    .is_complete()
            );
        }
    }
}

#[tokio::test]
async fn peer_deleted_project_reenters_real_engine_registration_without_sync() {
    for preserve_old_projection in [false, true] {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let old_project = f.local.get(&f.project_id).unwrap();
        f.delete_from_peer().await;
        let incoming = if preserve_old_projection {
            // Pod ensure clones existing metadata and its persistence identity.
            let mut incoming = (*old_project).clone();
            incoming.set_container(Some(f.observed.clone()));
            incoming
        } else {
            f.incoming(&f.project_id)
        };
        let registered = f
            .state
            .ensure_computer_project_mapping(
                &f.params(),
                Arc::new(incoming),
                RegistrationMetadata::PreserveChatMetadata,
            )
            .await
            .expect("peer-retired project must re-register via the original engine entry");
        assert_eq!(
            registered.container_info().unwrap().container_id,
            f.observed.container_id
        );
        assert_eq!(
            registered.container_info().unwrap().workload_uid,
            f.observed.workload_uid
        );
        assert_ne!(
            registered.persistence_identity().generation,
            old_project.persistence_identity().generation
        );
        assert!(
            registered.sessions().is_empty(),
            "retired sessions must not be resurrected"
        );
        assert_eq!(
            f.runtime
                .agent_replacement_verifications
                .load(Ordering::SeqCst),
            0
        );
        assert_eq!(
            f.state
                .append_session_for_container_durable(&old_project, "late-old-completion")
                .await
                .unwrap(),
            PersistenceWriteOutcome::Superseded
        );
        let persisted = f
            .peer
            .refresh_project_registration(&f.project_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            persisted.container_info().unwrap().container_id,
            f.observed.container_id
        );
        assert!(persisted.sessions().is_empty());
        f.shutdown().await;
    }
}

#[tokio::test]
async fn peer_deleted_canonical_project_does_not_block_a_new_shared_project() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    f.delete_from_peer().await;
    let requested = format!("new-{}", f.project_id);
    let registered = f
        .state
        .ensure_computer_project_mapping(
            &f.params(),
            Arc::new(f.incoming(&requested)),
            RegistrationMetadata::UpdateChatMetadata,
        )
        .await
        .expect("a stale retired canonical project must not block a fresh shared request");
    assert_eq!(registered.project_id(), requested);
    assert_eq!(
        registered.container_info().unwrap().container_id,
        f.observed.container_id
    );
    assert!(f.local.get(&f.project_id).is_none());
    assert_eq!(
        f.runtime
            .agent_replacement_verifications
            .load(Ordering::SeqCst),
        0
    );
    f.shutdown().await;
}

#[tokio::test]
async fn a_bound_external_workload_replacement_still_requires_runtime_ownership_proof() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    // No peer deletion/tombstones. The new runtime has failed ownership checks.
    *f.runtime.agent_replacement.lock().unwrap() = None;
    let error = f
        .state
        .ensure_computer_project_mapping(
            &f.params(),
            Arc::new(f.incoming(&f.project_id)),
            RegistrationMetadata::UpdateChatMetadata,
        )
        .await
        .err()
        .expect("an unverified external replacement must be rejected");
    assert!(format!("{error:#}").contains("runtime workspace owner mismatch"));
    let response = shared_types::computer_registration_app_error(error).into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["code"], shared_types::ERR_COMPUTER_OWNER_CONFLICT);
    assert_eq!(body["success"], false);
    assert_eq!(
        body["error_detail"]["reason_code"],
        shared_types::error_codes::CONTAINER_WORKLOAD_REPLACED
    );
    assert_eq!(
        body["error_detail"]["stage"],
        "computer.registration.before_dispatch"
    );
    assert_eq!(body["error_detail"]["retryable"], false);
    assert_eq!(
        f.runtime
            .agent_replacement_verifications
            .load(Ordering::SeqCst),
        1
    );
    assert_eq!(
        f.local
            .get(&f.project_id)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        f.old.container_id
    );
    assert_eq!(
        f.peer
            .refresh_project_registration(&f.project_id)
            .await
            .unwrap()
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        f.old.container_id
    );
    f.shutdown().await;
}

#[tokio::test]
async fn replacement_verification_transport_failure_keeps_its_original_error_class() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    f.runtime
        .agent_replacement_transport_fails
        .store(true, Ordering::SeqCst);
    let error = f
        .state
        .ensure_computer_project_mapping(
            &f.params(),
            Arc::new(f.incoming(&f.project_id)),
            RegistrationMetadata::UpdateChatMetadata,
        )
        .await
        .err()
        .expect("transport failure cannot authorize registration");
    assert!(matches!(
        error.downcast_ref::<ContainerRuntimeError>(),
        Some(ContainerRuntimeError::K8sError(message))
            if message == "injected verification transport failure"
    ));
    assert!(error.downcast_ref::<ContainerWorkloadReplaced>().is_none());
    let response = shared_types::computer_registration_app_error(error).into_response();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["code"], shared_types::ERR_INTERNAL_SERVER_ERROR);
    assert_eq!(body["success"], false);
    assert_ne!(
        body["error_detail"]["reason_code"],
        shared_types::error_codes::CONTAINER_WORKLOAD_REPLACED
    );
    assert_eq!(
        f.runtime
            .agent_replacement_verifications
            .load(Ordering::SeqCst),
        1
    );
    assert_eq!(
        f.local
            .get(&f.project_id)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        f.old.container_id
    );
    f.shutdown().await;
}

#[tokio::test]
async fn peer_retirement_does_not_authorize_a_request_with_a_different_owner() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    f.delete_from_peer().await;
    let mut incoming = f.incoming(&f.project_id);
    incoming.set_user_id(Some("different-owner".into()));
    let error = f
        .state
        .ensure_computer_project_mapping(
            &f.params(),
            Arc::new(incoming),
            RegistrationMetadata::UpdateChatMetadata,
        )
        .await
        .err()
        .expect("a mismatched request owner must be rejected");
    assert!(error.downcast_ref::<ComputerOwnerMismatch>().is_some());
    assert_eq!(
        f.runtime
            .agent_replacement_verifications
            .load(Ordering::SeqCst),
        0
    );
    f.shutdown().await;
}
