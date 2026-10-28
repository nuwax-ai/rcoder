//! Real manager control flow against a controlled asynchronous runtime.
use super::*;
use async_trait::async_trait;
use container_runtime_api::{
    AgentContainerRuntime, ContainerRuntimeStatus, RuntimeContainerInfo, UserAppDeploymentRuntime,
    WorkspaceRuntime,
};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct Runtime {
    status: Mutex<ContainerRuntimeStatus>,
    creates: AtomicUsize,
    stops: AtomicUsize,
    observation_failed: AtomicBool,
    cached: Mutex<Option<ContainerBasicInfo>>,
    fresh_queries: AtomicUsize,
    stale_info_reads: Mutex<usize>,
}

impl Runtime {
    fn new(status: ContainerRuntimeStatus) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(status),
            creates: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
            observation_failed: AtomicBool::new(false),
            cached: Mutex::new(None),
            fresh_queries: AtomicUsize::new(0),
            stale_info_reads: Mutex::new(0),
        })
    }

    fn info(&self) -> ContainerBasicInfo {
        ContainerBasicInfo {
            container_id: "pending-pod-uid".into(),
            container_name: "computer-agent-runner-pending".into(),
            container_ip: "127.0.0.1".into(),
            internal_port: 8888,
            external_port: 0,
            project_id: "pending-project".into(),
            status: String::from(self.status.lock().unwrap().clone()),
            created_at: chrono::Utc::now(),
            service_url: "http://127.0.0.1:8888".into(),
            workload_uid: Some("retained-statefulset-uid".into()),
        }
    }

    fn cached_info(&self) -> ContainerBasicInfo {
        if let Some(cached) = self.cached.lock().unwrap().clone() {
            return cached;
        }
        let mut remaining = self.stale_info_reads.lock().unwrap();
        if *remaining > 0 {
            *remaining -= 1;
            let mut stale = self.info();
            stale.container_id = "externally-deleted-pod".into();
            return stale;
        }
        self.info()
    }
}

#[async_trait]
impl AgentContainerRuntime for Runtime {
    async fn create_container(
        &self,
        _: ContainerCreateParams,
    ) -> container_runtime_api::ContainerRuntimeResult<ContainerBasicInfo> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        Err(
            container_runtime_api::ContainerRuntimeError::ContainerCreationError(
                "unexpected creation of an existing workload".into(),
            ),
        )
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> container_runtime_api::ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(Some(self.cached_info()))
    }
    async fn get_container_info_by_identifier(
        &self,
        _: &str,
        _: &ServiceType,
    ) -> container_runtime_api::ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(Some(self.cached_info()))
    }
    async fn find_container(
        &self,
        _: &str,
        service_type: &ServiceType,
    ) -> container_runtime_api::ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        if self.observation_failed.load(Ordering::SeqCst) {
            return Err(
                container_runtime_api::ContainerRuntimeError::ConnectionError(
                    "controlled pending observation failure".into(),
                ),
            );
        }
        let info = self
            .cached
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.info());
        Ok(Some(RuntimeContainerInfo {
            container_id: info.container_id,
            container_name: info.container_name,
            container_ip: info.container_ip,
            status: self.status.lock().unwrap().clone(),
            created_at: info.created_at,
            env_vars: None,
            service_type: Some(*service_type),
            project_id: None,
            user_id: Some("pending-user".into()),
            pod_id: Some("pending-pod".into()),
            app_id: None,
            workload_uid: info.workload_uid,
        }))
    }
    async fn find_container_for_ensure(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> container_runtime_api::ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        self.fresh_queries.fetch_add(1, Ordering::SeqCst);
        *self.cached.lock().unwrap() = None;
        self.find_container(identifier, service_type).await
    }
    async fn stop_container(&self, _: &str) -> container_runtime_api::ContainerRuntimeResult<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn is_container_running(
        &self,
        _: &str,
    ) -> container_runtime_api::ContainerRuntimeResult<bool> {
        Ok(*self.status.lock().unwrap() == ContainerRuntimeStatus::Running)
    }
    async fn list_containers(
        &self,
    ) -> container_runtime_api::ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> container_runtime_api::ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn health_check(&self) -> container_runtime_api::ContainerRuntimeResult<()> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn ensure_inside_cache_ttl_uses_current_pod_before_dispatch() {
    let runtime = Runtime::new(ContainerRuntimeStatus::Running);
    let mut stale = runtime.info();
    stale.container_id = "externally-deleted-pod".into();
    *runtime.cached.lock().unwrap() = Some(stale);
    let service: Arc<dyn ContainerRuntime> = runtime.clone();
    let info = ComputerContainerManager::get_or_create_container_for_user_with_type(
        &options("cache-replaced-pod"),
        &service,
    )
    .await
    .unwrap();
    assert_eq!(info.container_id, "pending-pod-uid");
    assert!(runtime.fresh_queries.load(Ordering::SeqCst) > 0);
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.stops.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn ensure_reobserves_intervening_address_snapshot_within_original_budget() {
    let runtime = Runtime::new(ContainerRuntimeStatus::Running);
    *runtime.stale_info_reads.lock().unwrap() = 1;
    let service: Arc<dyn ContainerRuntime> = runtime.clone();
    let info = ComputerContainerManager::get_or_create_container_for_user_with_type(&options("cache-address-race"), &service).await
        .expect("a changed address snapshot must converge in the original ensure, without prompting the caller to replay");
    assert_eq!(info.container_id, "pending-pod-uid");
    assert!(runtime.fresh_queries.load(Ordering::SeqCst) >= 2);
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.stops.load(Ordering::SeqCst), 0);
}
#[async_trait]
impl WorkspaceRuntime for Runtime {}
#[async_trait]
impl UserAppDeploymentRuntime for Runtime {}

fn options(name: &str) -> ContainerCreateOptions {
    ContainerCreateOptions {
        user_id: "pending-user".into(),
        project_id: "pending-project".into(),
        resource_limits: None,
        pod_id: Some(name.into()),
        isolation_type: None,
        tenant_id: None,
        space_id: None,
        service_type: ServiceType::ComputerAgentRunner,
    }
}

#[tokio::test(start_paused = true)]
async fn pending_workload_is_waited_without_stopping() {
    let runtime = Runtime::new(ContainerRuntimeStatus::Pending);
    tokio::spawn({
        let runtime = runtime.clone();
        async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            *runtime.status.lock().unwrap() = ContainerRuntimeStatus::Running;
        }
    });
    let service: Arc<dyn ContainerRuntime> = runtime.clone();
    let result = ComputerContainerManager::get_or_create_container_for_user_with_type(
        &options("pending-no-stop"),
        &service,
    )
    .await;
    assert_eq!(
        runtime.stops.load(Ordering::SeqCst),
        0,
        "Pending was deleted"
    );
    let info = result.expect("existing creation must finish");
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    assert_eq!(info.container_id, "pending-pod-uid");
    assert_eq!(
        info.workload_uid.as_deref(),
        Some("retained-statefulset-uid")
    );
    assert_eq!(info.status, "running");
}

#[tokio::test(start_paused = true)]
async fn concurrent_pending_ensures_do_not_delete_after_wait_budget() {
    let runtime = Runtime::new(ContainerRuntimeStatus::Pending);
    let service: Arc<dyn ContainerRuntime> = runtime.clone();
    let request = options("pending-old-marker");
    let first = tokio::spawn({
        let service = service.clone();
        let request = request.clone();
        async move {
            ComputerContainerManager::get_or_create_container_for_user_with_type(&request, &service)
                .await
        }
    });
    tokio::task::yield_now().await;
    let second = tokio::spawn({
        let service = service.clone();
        let request = request.clone();
        async move {
            ComputerContainerManager::get_or_create_container_for_user_with_type(&request, &service)
                .await
        }
    });
    tokio::task::yield_now().await;
    // An elapsed waiter budget / the old 60s marker TTL proves no physical exit.
    tokio::time::advance(std::time::Duration::from_secs(161)).await;
    assert!(first.await.unwrap().is_err());
    assert!(second.await.unwrap().is_err());
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime.stops.load(Ordering::SeqCst),
        0,
        "waiting triggered a deletion"
    );
    *runtime.status.lock().unwrap() = ContainerRuntimeStatus::Running;
    let resumed =
        ComputerContainerManager::get_or_create_container_for_user_with_type(&request, &service)
            .await
            .unwrap();
    assert_eq!(resumed.container_id, "pending-pod-uid");
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.stops.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unknown_workload_is_not_deleted() {
    let runtime = Runtime::new(ContainerRuntimeStatus::Unknown("unavailable".into()));
    let service: Arc<dyn ContainerRuntime> = runtime.clone();
    let result = ComputerContainerManager::get_or_create_container_for_user_with_type(
        &options("unknown-no-stop"),
        &service,
    )
    .await;
    assert_eq!(
        runtime.stops.load(Ordering::SeqCst),
        0,
        "unknown state was deleted"
    );
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    assert!(result.is_err());
}

#[tokio::test(start_paused = true)]
async fn pending_observation_failure_does_not_authorize_replacement() {
    let runtime = Runtime::new(ContainerRuntimeStatus::Pending);
    tokio::spawn({
        let runtime = runtime.clone();
        async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            runtime.observation_failed.store(true, Ordering::SeqCst);
        }
    });
    let service: Arc<dyn ContainerRuntime> = runtime.clone();
    let error = ComputerContainerManager::get_or_create_container_for_user_with_type(
        &options("pending-observation-error"),
        &service,
    )
    .await
    .unwrap_err();
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.stops.load(Ordering::SeqCst), 0);
    match error {
        AppError::Structured(detail) => {
            assert_eq!(detail.code, shared_types::ERR_RUNTIME_UNAVAILABLE)
        }
        other => panic!("expected typed observation error, got {other}"),
    }
}
