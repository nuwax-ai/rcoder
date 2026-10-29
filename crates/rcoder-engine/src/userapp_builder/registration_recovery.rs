//! Controlled recovery of a MISSING canonical builder registration.
//!
//! A live UserApp builder whose `projects` row is absent is invisible to the
//! ordinary idle scanner (it only walks `projects`), so the reverse inventory
//! currently protects it forever as `LifecycleReferenced`. This module closes
//! that gap without creating a second reaper and without weakening ownership:
//!
//! * authority is either a **successful, identity-bound creation/Ensure
//!   receipt** (the existing durable chain) or a **valid resource binding for
//!   the current lifecycle** — the same evidence that protects the workload
//!   today;
//! * nothing is created, started, restarted or migrated: the physical Pod,
//!   controller, PVC and data are only observed;
//! * insufficient evidence returns `Ok(None)`; the caller retains the
//!   resource and reports the specific gap. Missing evidence is neither
//!   permission to adopt nor permission to delete.
use super::{adoption, creation, validate_builder_identity};
use crate::app_state::AppState;
use anyhow::{Context, Result, anyhow};
use shared_types::{
    AGENT_FILE_SERVER_PORT, ContainerBasicInfo, ProjectAndContainerInfo, ProjectStore as _,
    ServiceType, UserAppLifecycleState, persistence::PersistenceWriteOutcome,
};
use std::sync::Arc;

/// 补齐缺失的 canonical builder 登记。`Some` = 登记已落库（可被普通 scanner
/// 遍历）；`None` = 本轮证据不足，资源保留并需显式受控恢复入口。
pub(crate) async fn recover_missing_builder_registration(
    state: &AppState,
    app_id: &str,
) -> Result<Option<ContainerBasicInfo>> {
    let app = state
        .userapp_store
        .get_application(app_id)
        .await?
        .ok_or_else(|| anyhow!("Builder registration recovery lifecycle missing: {app_id}"))?;
    if app.state != UserAppLifecycleState::Active {
        return Ok(None);
    }
    // Read-only runtime observation: the same source the ordinary path uses.
    let Some(observed) = state
        .runtime()
        .find_container(app_id, &ServiceType::UserappBuilder)
        .await
        .context("inspect builder without canonical registration")?
    else {
        return Ok(None);
    };
    validate_builder_identity(app_id, &observed)?;
    if observed.status != container_runtime_api::ContainerRuntimeStatus::Running {
        return Ok(None);
    }
    // Real runtime identity, current application/lifecycle and workload UID.
    adoption::verify_live_builder_observation(state, app_id, app_id, &observed).await?;
    // A live Pod without its management Service is incomplete routing, not a
    // missing row: repair belongs to admitted creation/resume, which owns the
    // durable operation and runtime lease.
    if state
        .runtime()
        .get_container_info_by_identifier(app_id, &ServiceType::UserappBuilder)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    let verified = ContainerBasicInfo {
        container_id: observed.container_id.clone(),
        container_name: observed.container_name.clone(),
        container_ip: observed.container_ip.clone(),
        internal_port: AGENT_FILE_SERVER_PORT,
        external_port: 0,
        project_id: app_id.into(),
        status: String::from(observed.status.clone()),
        created_at: observed.created_at,
        service_url: format!(
            "http://{}:{}",
            observed.container_ip, AGENT_FILE_SERVER_PORT
        ),
        workload_uid: observed.workload_uid.clone(),
    };
    // Shared SQL registry: only a succeeded, identity-bound Ensure receipt may
    // publish the registration, through the existing transactional chain.
    match creation::repair_live_registration(state, app_id, app_id, &verified).await {
        Ok(true) => return Ok(Some(verified)),
        Ok(false) => {}
        Err(error) if error.is::<creation::BuilderRegistrationConfirmationRequired>() => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    }
    // Ordinary backend: authority is the binding of the CURRENT lifecycle to
    // the observed controller — the same evidence that protects this workload.
    if !bound_to_current_lifecycle(state, app_id, &app.lifecycle_id, &verified).await? {
        return Ok(None);
    }
    // A first-ever registration starts its own idle window; re-observing an
    // already registered instance would keep its historical clock instead
    // (same rule as `register_builder`).
    let mut project = state
        .projects
        .get(app_id)
        .map(|old| (*old).clone())
        .unwrap_or_else(|| ProjectAndContainerInfo::new(app_id.to_owned()));
    if project
        .container_info()
        .is_none_or(|previous| previous.container_id != verified.container_id)
    {
        project.update_activity();
    }
    project.set_service_type(Some(ServiceType::UserappBuilder));
    project.set_container(Some(verified.clone()));
    let outcome = state
        .projects
        .insert_project_registration_durable(app_id.into(), Arc::new(project))
        .await?;
    Ok((outcome == PersistenceWriteOutcome::Committed).then_some(verified))
}

/// 同一当前生命周期对该 controller 的有效绑定。缺绑定、跨应用或跨代次的绑定
/// 都不构成补齐登记的依据。
async fn bound_to_current_lifecycle(
    state: &AppState,
    app_id: &str,
    lifecycle_id: &str,
    verified: &ContainerBasicInfo,
) -> Result<bool> {
    let Some(uid) = verified
        .workload_uid
        .as_deref()
        .filter(|uid| !uid.is_empty())
    else {
        return Ok(false);
    };
    Ok(state
        .userapp_store
        .get_resource_binding(&ServiceType::UserappBuilder, uid)
        .await?
        .is_some_and(|binding| {
            binding.app_id == app_id
                && binding.lifecycle_id == lifecycle_id
                && binding.service_type == ServiceType::UserappBuilder
        }))
}

#[cfg(all(test, feature = "rcoder-pg"))]
mod pg_tests {
    use super::*;
    use container_runtime_api::{
        AgentContainerRuntime, ContainerRuntimeResult, RuntimeContainerInfo,
        UserAppDeploymentRuntime, WorkspaceRuntime,
    };

    const DSN_ENV: &str = "RCODER_PG_TEST_DSN";

    /// 真实 PG 下的受控恢复：登记必须落库，并且**另一个独立连接**能读到
    /// （跨副本可见性），而不是仅本副本内存插入。
    #[tokio::test]
    async fn recovered_registration_is_durable_and_visible_to_another_replica() {
        let dsn = std::env::var(DSN_ENV)
            .ok()
            .filter(|value| !value.is_empty())
            .expect("explicit PG contract requires an isolated database");
        let app_id = format!("builder-recover-{}", uuid::Uuid::new_v4().simple());
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &directory.path().join("recovery.db"),
            )
            .await
            .unwrap(),
        );
        let runtime = Arc::new(Runtime {
            app_id: app_id.clone(),
            pod_uid: format!("pod-{}", uuid::Uuid::new_v4().simple()),
            workload_uid: format!("sts-{}", uuid::Uuid::new_v4().simple()),
        });
        let mut state =
            (*crate::userapp_builder::builder_runtime_test_state(runtime.clone(), store, None)
                .await)
                .clone();
        let config = rcoder_storage::config::PostgresConfig {
            url: Some(dsn.clone()),
            ..Default::default()
        };
        let backend = Arc::new(rcoder_storage::ProjectStoreBackend::Postgres(Arc::new(
            rcoder_storage::pg::PgStore::connect(&config, "test-ns".into(), "cluster.local".into())
                .await
                .unwrap(),
        )));
        state.projects = backend;
        // 独立 UserApp 控制库（与 Agent 注册表不同库）：`requires_durable_confirmation`
        // 为 false，权威来自当前生命周期绑定，登记仍必须落库并对另一副本可见。
        state.config.userapp_storage.postgres = Some(rcoder_storage::config::PostgresConfig {
            url: Some(format!(
                "postgres://testpg:testpg@127.0.0.1:32768/idle_control_{}",
                uuid::Uuid::new_v4().simple()
            )),
            ..Default::default()
        });
        let state = Arc::new(state);

        let lifecycle = state.userapp_store.ensure_identity(&app_id).await.unwrap();
        // 当前生命周期绑定（今天保护该 workload 的同一证据）。
        let input = shared_types::UserAppExecutionInput::new(
            serde_json::to_string(&shared_types::AdoptBuilderRequest {
                lifecycle_id: lifecycle.lifecycle_id.clone(),
                request_id: "recovery-request".into(),
                expected_container_id: runtime.pod_uid.clone(),
            })
            .unwrap(),
        );
        let admitted = match state
            .userapp_store
            .admit_with_input(
                &shared_types::UserAppAdmission {
                    app_id: app_id.clone(),
                    lifecycle_id: Some(lifecycle.lifecycle_id.clone()),
                    request_id: Some("recovery-request".into()),
                    operation_id: "recovery-binding".into(),
                    request_fingerprint: input.digest(),
                    kind: shared_types::UserAppOperationKind::AdoptBuilder,
                    command: None,
                    metadata: None,
                    runtime_policy_on_success: None,
                },
                Some(&input),
            )
            .await
            .unwrap()
        {
            shared_types::UserAppAdmissionOutcome::Accepted(record) => record,
            other => panic!("fresh fixture admission: {other:?}"),
        };
        let running = state
            .userapp_store
            .advance(&shared_types::UserAppOperationProgress {
                app_id: admitted.app_id.clone(),
                lifecycle_id: admitted.lifecycle_id.clone(),
                operation_id: admitted.operation_id.clone(),
                expected_revision: admitted.revision,
                executor_id: "fixture".into(),
                state: shared_types::UserAppOperationState::Running,
                step: "claimed".into(),
                checkpoint: serde_json::Value::Null,
                error_code: None,
                error_message: None,
            })
            .await
            .unwrap();
        state
            .userapp_store
            .commit_resource_binding(
                &shared_types::UserAppResourceBinding {
                    app_id: running.app_id.clone(),
                    lifecycle_id: running.lifecycle_id.clone(),
                    service_type: ServiceType::UserappBuilder,
                    physical_uid: runtime.workload_uid.clone(),
                    adopted_by_operation: running.operation_id.clone(),
                },
                &shared_types::UserAppOperationProgress {
                    app_id: running.app_id.clone(),
                    lifecycle_id: running.lifecycle_id.clone(),
                    operation_id: running.operation_id.clone(),
                    expected_revision: running.revision,
                    executor_id: "fixture".into(),
                    state: shared_types::UserAppOperationState::Succeeded,
                    step: "adopted".into(),
                    checkpoint: serde_json::Value::Null,
                    error_code: None,
                    error_message: None,
                },
            )
            .await
            .unwrap();

        let recovered = recover_missing_builder_registration(&state, &app_id)
            .await
            .unwrap()
            .expect("current-lifecycle binding must complete the registration");
        assert_eq!(recovered.container_id, runtime.pod_uid);
        assert_eq!(
            recovered.workload_uid.as_deref(),
            Some(runtime.workload_uid.as_str())
        );

        // 另一个副本的独立连接必须能读到同一登记（不是仅内存插入）。
        let peer =
            rcoder_storage::pg::PgStore::connect(&config, "test-ns".into(), "cluster.local".into())
                .await
                .unwrap();
        let peer_view = peer.get(&app_id).expect("peer replica must read the row");
        let peer_container = peer_view.container_info().expect("builder container");
        assert_eq!(peer_container.container_id, runtime.pod_uid);
        assert_eq!(peer_view.service_type(), Some(ServiceType::UserappBuilder));
    }

    struct Runtime {
        app_id: String,
        pod_uid: String,
        workload_uid: String,
    }

    #[async_trait::async_trait]
    impl AgentContainerRuntime for Runtime {
        fn agent_workload_name(
            &self,
            identifier: &str,
            _: &ServiceType,
        ) -> ContainerRuntimeResult<String> {
            Ok(identifier.into())
        }
        fn agent_workload_namespace(&self) -> String {
            "test-ns".into()
        }
        async fn find_container(
            &self,
            _: &str,
            family: &ServiceType,
        ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
            if *family != ServiceType::UserappBuilder {
                return Ok(None);
            }
            Ok(Some(RuntimeContainerInfo {
                container_id: self.pod_uid.clone(),
                container_name: self.app_id.clone(),
                container_ip: "10.42.4.207".into(),
                status: container_runtime_api::ContainerRuntimeStatus::Running,
                created_at: chrono::Utc::now(),
                env_vars: None,
                service_type: Some(ServiceType::UserappBuilder),
                project_id: Some(self.app_id.clone()),
                user_id: None,
                pod_id: None,
                app_id: Some(self.app_id.clone()),
                workload_uid: Some(self.workload_uid.clone()),
            }))
        }
        async fn inspect_builder_candidate(
            &self,
            context: &shared_types::UserAppExecutionContext,
        ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
            self.target(context)
        }
        async fn capture_builder_control(
            &self,
            context: &shared_types::UserAppExecutionContext,
        ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
            self.target(context)
        }
        async fn capture_bound_builder_control(
            &self,
            context: &shared_types::UserAppExecutionContext,
            _binding: Option<&shared_types::UserAppResourceBinding>,
        ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
            self.target(context)
        }
        async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
            panic!("registration recovery must not stop compute")
        }
        async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
            Ok(true)
        }
        async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
            Ok(vec![])
        }
        async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
            panic!("registration recovery must not delete resources")
        }
        async fn health_check(&self) -> ContainerRuntimeResult<()> {
            Ok(())
        }
        async fn sync_states(
            &self,
        ) -> ContainerRuntimeResult<(u32, Vec<container_runtime_api::RemovedContainerInfo>)>
        {
            Ok((0, vec![]))
        }
        async fn create_container(
            &self,
            _: container_runtime_api::ContainerCreateParams,
        ) -> ContainerRuntimeResult<ContainerBasicInfo> {
            panic!("registration recovery must not create compute")
        }
        async fn get_container_info(
            &self,
            _: &str,
        ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
            Ok(None)
        }
    }

    impl Runtime {
        fn target(
            &self,
            context: &shared_types::UserAppExecutionContext,
        ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
            Ok(shared_types::BuilderControlTarget {
                context: context.clone(),
                resource_binding: None,
                workload: Some(shared_types::AppResourceIdentity {
                    kind: shared_types::AppResourceKind::StatefulSet,
                    name: self.app_id.clone(),
                    uid: self.workload_uid.clone(),
                    resource_version: Some("1".into()),
                }),
                pod: Some(shared_types::BuilderPodIdentity {
                    name: format!("{}-0", self.app_id),
                    uid: self.pod_uid.clone(),
                    resource_version: "2".into(),
                }),
                restart_image: None,
                restart_runtime_workspace: None,
                builder_startup_mode: Default::default(),
            })
        }
    }

    #[async_trait::async_trait]
    impl WorkspaceRuntime for Runtime {}
    #[async_trait::async_trait]
    impl UserAppDeploymentRuntime for Runtime {
        async fn list_deployments(
            &self,
        ) -> ContainerRuntimeResult<Vec<container_runtime_api::DeploymentStatus>> {
            Ok(vec![])
        }
    }

    /// 缺证据（无当前生命周期绑定）时不得写登记——真实 PG 下同样保持保留。
    #[tokio::test]
    async fn recovered_registration_requires_current_lifecycle_binding() {
        let dsn = std::env::var(DSN_ENV)
            .ok()
            .filter(|value| !value.is_empty())
            .expect("explicit PG contract requires an isolated database");
        let app_id = format!("builder-nobind-{}", uuid::Uuid::new_v4().simple());
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &directory.path().join("nobind.db"),
            )
            .await
            .unwrap(),
        );
        let workload_uid = format!("sts-{}", uuid::Uuid::new_v4().simple());
        let runtime = Arc::new(Runtime {
            app_id: app_id.clone(),
            pod_uid: format!("pod-{}", uuid::Uuid::new_v4().simple()),
            workload_uid,
        });
        let mut state = (*crate::userapp_builder::builder_runtime_test_state(runtime, store, None)
            .await)
            .clone();
        let config = rcoder_storage::config::PostgresConfig {
            url: Some(dsn),
            ..Default::default()
        };
        state.projects = Arc::new(rcoder_storage::ProjectStoreBackend::Postgres(Arc::new(
            rcoder_storage::pg::PgStore::connect(&config, "test-ns".into(), "cluster.local".into())
                .await
                .unwrap(),
        )));
        let state = Arc::new(state);
        state.userapp_store.ensure_identity(&app_id).await.unwrap();
        assert!(
            recover_missing_builder_registration(&state, &app_id)
                .await
                .unwrap()
                .is_none(),
            "no current-lifecycle binding must not complete a registration"
        );
    }
}
