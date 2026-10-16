//! Inventory hints feed the same read-only lifecycle restoration as first access.
//! A PVC/name/cache entry is never passed to storage as ownership evidence.
use super::*;
use std::collections::{BTreeSet, VecDeque};

pub(super) struct Discovery {
    queue: VecDeque<String>,
    refresh_at: tokio::time::Instant,
}
impl Default for Discovery {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            refresh_at: tokio::time::Instant::now(),
        }
    }
}
impl Discovery {
    pub(super) async fn poll(
        &mut self,
        state: &std::sync::Arc<AppState>,
        tasks: &mut RecoveryTasks,
    ) -> anyhow::Result<()> {
        if tasks.is_full() {
            return Ok(());
        }
        if self.queue.is_empty() && tokio::time::Instant::now() >= self.refresh_at {
            self.refresh_at = tokio::time::Instant::now() + Duration::from_secs(60);
            let runtime = state.runtime();
            let (containers, dev, prod, deployments) =
                tokio::time::timeout(SCAN_READ_TIMEOUT, async {
                    tokio::try_join!(
                        runtime.list_containers(),
                        runtime
                            .list_workspace_identifiers(&shared_types::ServiceType::UserappBuilder),
                        runtime.list_workspace_identifiers(&shared_types::ServiceType::Userapp),
                        runtime.list_deployments()
                    )
                })
                .await
                .map_err(|_| anyhow::anyhow!("Lifecycle discovery inventory timed out"))??;
            let mut ids: BTreeSet<String> = dev.into_iter().chain(prod).collect();
            // Docker's Agent inventory is an in-memory cache and its prod
            // workspace scan is empty. Deployment inventory queries Docker
            // itself, so a controller restart cannot hide live prod compute.
            // These IDs remain hints: restoration still confirms native
            // lifecycle/physical identity twice before writing the control root.
            ids.extend(deployments.into_iter().map(|deployment| deployment.app_id));
            for info in containers {
                if matches!(
                    info.service_type,
                    Some(
                        shared_types::ServiceType::Userapp
                            | shared_types::ServiceType::UserappBuilder
                    )
                ) && let Some(id) = info.app_id
                {
                    ids.insert(id);
                }
            }
            self.queue.extend(ids);
        }
        for _ in 0..2 {
            if tasks.is_full() {
                break;
            }
            let Some(app_id) = self.queue.pop_front() else {
                break;
            };
            let state = state.clone();
            tasks.push(format!("discovery:{app_id}"), async move {
                tokio::time::timeout(
                    Duration::from_secs(30),
                    state.app_service.discover_missing_identity(&app_id),
                )
                .await
                .map_err(|_| anyhow::anyhow!("Lifecycle restoration timed out"))??;
                Ok(())
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use container_runtime_api::{
        AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError,
        ContainerRuntimeResult, DeploymentStatus, RuntimeContainerInfo, UserAppDeploymentRuntime,
        WorkspaceRuntime,
    };
    use shared_types::{ContainerBasicInfo, ServiceType, UserAppDiscoveredIdentity};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    const APP: &str = "live-prod";
    const LIFECYCLE: &str = "original-lifecycle";

    #[derive(Clone, Copy)]
    enum NativeObservation {
        Stable,
        ChangedOnConfirmation,
        Unavailable,
    }

    struct Runtime {
        inventory_unavailable: AtomicBool,
        native: Mutex<NativeObservation>,
        native_reads: AtomicUsize,
        mutations: AtomicUsize,
    }

    impl Runtime {
        fn identity() -> UserAppDiscoveredIdentity {
            UserAppDiscoveredIdentity {
                app_id: APP.into(),
                lifecycle_id: LIFECYCLE.into(),
                dev_uid: None,
                prod_uid: Some("original-physical-uid".into()),
                dev_stopped: false,
                prod_stopped: false,
                dev_volumes: vec![],
                prod_volumes: vec![],
            }
        }

        fn reject_mutation(&self) -> ContainerRuntimeError {
            self.mutations.fetch_add(1, Ordering::SeqCst);
            ContainerRuntimeError::ConfigurationError("discovery must not mutate compute".into())
        }
    }

    #[async_trait::async_trait]
    impl AgentContainerRuntime for Runtime {
        async fn create_container(
            &self,
            _: ContainerCreateParams,
        ) -> ContainerRuntimeResult<ContainerBasicInfo> {
            Err(self.reject_mutation())
        }

        async fn get_container_info(
            &self,
            _: &str,
        ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
            Ok(None)
        }

        async fn find_container(
            &self,
            _: &str,
            _: &ServiceType,
        ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
            Ok(None)
        }

        async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
            Err(self.reject_mutation())
        }

        async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
            Ok(false)
        }

        async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
            // A fresh Docker controller has no Agent cache entries.
            Ok(vec![])
        }

        async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
            Err(self.reject_mutation())
        }

        async fn health_check(&self) -> ContainerRuntimeResult<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl WorkspaceRuntime for Runtime {
        async fn list_workspace_identifiers(
            &self,
            _: &ServiceType,
        ) -> ContainerRuntimeResult<Vec<String>> {
            Ok(vec![])
        }
    }

    #[async_trait::async_trait]
    impl UserAppDeploymentRuntime for Runtime {
        async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
            if self.inventory_unavailable.load(Ordering::SeqCst) {
                return Err(ContainerRuntimeError::ConnectionError(
                    "deployment inventory unavailable".into(),
                ));
            }
            Ok(vec![DeploymentStatus {
                app_id: APP.into(),
                lifecycle_id: Some(LIFECYCLE.into()),
                phase: "Running".into(),
                replicas: 1,
                ready_replicas: 1,
                ..Default::default()
            }])
        }

        async fn discover_application_identity(
            &self,
            app_id: &str,
        ) -> ContainerRuntimeResult<Option<UserAppDiscoveredIdentity>> {
            assert_eq!(app_id, APP);
            let read = self.native_reads.fetch_add(1, Ordering::SeqCst);
            let observation = *self.native.lock().unwrap();
            if matches!(observation, NativeObservation::Unavailable) {
                return Err(ContainerRuntimeError::ConnectionError(
                    "native identity observation unavailable".into(),
                ));
            }
            let mut found = Self::identity();
            if matches!(observation, NativeObservation::ChangedOnConfirmation) && read > 0 {
                found.prod_uid = Some("replacement-physical-uid".into());
            }
            Ok(Some(found))
        }
    }

    async fn fixture(
        observation: NativeObservation,
    ) -> (Arc<AppState>, Arc<Runtime>, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &directory.path().join("userapp.db"),
            )
            .await
            .unwrap(),
        );
        let runtime = Arc::new(Runtime {
            inventory_unavailable: AtomicBool::new(false),
            native: Mutex::new(observation),
            native_reads: AtomicUsize::new(0),
            mutations: AtomicUsize::new(0),
        });
        let state = crate::userapp_builder::creation::fence_settler_tests::runtime_state(
            runtime.clone(),
            store,
            None,
        )
        .await;
        assert!(
            state
                .userapp_store
                .get_application(APP)
                .await
                .unwrap()
                .is_none()
        );
        (state, runtime, directory)
    }

    #[tokio::test]
    async fn live_prod_inventory_restores_missing_root_with_empty_agent_cache() {
        let (state, runtime, _directory) = fixture(NativeObservation::Stable).await;
        let mut discovery = Discovery::default();
        let mut tasks = RecoveryTasks::default();
        discovery.poll(&state, &mut tasks).await.unwrap();
        assert_eq!(tasks.active.len(), 1);
        let (id, result) = tasks.next().await.unwrap();
        assert_eq!(id, format!("discovery:{APP}"));
        result.unwrap();
        let restored = state
            .userapp_store
            .get_application(APP)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restored.lifecycle_id, LIFECYCLE);
        assert_eq!(
            state
                .userapp_store
                .get_recovery_witness(APP, LIFECYCLE)
                .await
                .unwrap(),
            Some(Runtime::identity())
        );
        assert_eq!(runtime.native_reads.load(Ordering::SeqCst), 2);
        assert_eq!(runtime.mutations.load(Ordering::SeqCst), 0);
        assert!(tasks.active.is_empty());
    }

    #[tokio::test]
    async fn changed_native_uid_during_confirmation_does_not_restore_a_root() {
        let (state, runtime, _directory) = fixture(NativeObservation::ChangedOnConfirmation).await;
        let mut tasks = RecoveryTasks::default();
        Discovery::default().poll(&state, &mut tasks).await.unwrap();
        let (_, result) = tasks.next().await.unwrap();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("inventory changed")
        );
        assert!(
            state
                .userapp_store
                .get_application(APP)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(runtime.native_reads.load(Ordering::SeqCst), 2);
        assert_eq!(runtime.mutations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn native_observation_failure_is_not_application_absence() {
        let (state, runtime, _directory) = fixture(NativeObservation::Unavailable).await;
        let mut tasks = RecoveryTasks::default();
        Discovery::default().poll(&state, &mut tasks).await.unwrap();
        let (_, result) = tasks.next().await.unwrap();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("native identity observation unavailable")
        );
        assert!(
            state
                .userapp_store
                .get_application(APP)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(runtime.native_reads.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.mutations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn deployment_inventory_failure_is_not_an_empty_inventory() {
        let (state, runtime, _directory) = fixture(NativeObservation::Stable).await;
        runtime.inventory_unavailable.store(true, Ordering::SeqCst);
        let mut tasks = RecoveryTasks::default();
        let error = Discovery::default()
            .poll(&state, &mut tasks)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("deployment inventory unavailable")
        );
        assert!(tasks.active.is_empty());
        assert_eq!(runtime.native_reads.load(Ordering::SeqCst), 0);
        assert!(
            state
                .userapp_store
                .get_application(APP)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(runtime.mutations.load(Ordering::SeqCst), 0);
    }
}
