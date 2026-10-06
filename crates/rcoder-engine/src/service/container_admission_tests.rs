//! Actual manager admission: uncertain observation never authorizes creation.
use super::{computer_container_manager, container_manager};
use async_trait::async_trait;
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntime, ContainerRuntimeError,
    ContainerRuntimeResult, RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use shared_types::{ContainerBasicInfo, ServiceType};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Copy)]
enum Scenario {
    QueryFailed,
    Missing,
    LiveWithoutAddress,
    StopUnknown,
    StatusUnknown,
}

struct Runtime {
    scenario: Scenario,
    creates: AtomicUsize,
    stops: AtomicUsize,
}

impl Runtime {
    fn new(scenario: Scenario) -> Arc<Self> {
        Arc::new(Self {
            scenario,
            creates: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        })
    }
    fn info(&self) -> ContainerBasicInfo {
        ContainerBasicInfo {
            container_id: "captured-container".into(),
            container_name: "test-container".into(),
            container_ip: if matches!(self.scenario, Scenario::LiveWithoutAddress) {
                String::new()
            } else {
                "127.0.0.1".into()
            },
            internal_port: 8086,
            external_port: 0,
            project_id: "project".into(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: "http://127.0.0.1".into(),
            workload_uid: None,
        }
    }
    fn query(&self) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        match self.scenario {
            Scenario::QueryFailed => Err(ContainerRuntimeError::ConnectionError(
                "controlled query failed".into(),
            )),
            Scenario::Missing => Ok(None),
            _ => Ok(Some(self.info())),
        }
    }
}

#[async_trait]
impl AgentContainerRuntime for Runtime {
    async fn create_container(
        &self,
        _: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        Ok(self.info())
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.query()
    }
    async fn get_container_info_by_identifier(
        &self,
        _: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.query()
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn stop_container_by_identifier(
        &self,
        _: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Err(ContainerRuntimeError::Timeout(
            "controlled cleanup result unknown".into(),
        ))
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        Ok(false)
    }
    async fn is_container_running_by_identifier(
        &self,
        _: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<bool> {
        match self.scenario {
            Scenario::StatusUnknown => Err(ContainerRuntimeError::ConnectionError(
                "controlled status unavailable".into(),
            )),
            Scenario::LiveWithoutAddress => Ok(true),
            _ => Ok(false),
        }
    }
    async fn find_container(
        &self,
        _: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        Ok(None)
    }
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
}
#[async_trait]
impl WorkspaceRuntime for Runtime {}
#[async_trait]
impl UserAppDeploymentRuntime for Runtime {}

fn options() -> computer_container_manager::ContainerCreateOptions {
    computer_container_manager::ContainerCreateOptions {
        user_id: "test-user".into(),
        project_id: "project".into(),
        resource_limits: None,
        pod_id: Some("isolated-pod".into()),
        isolation_type: None,
        tenant_id: None,
        space_id: None,
        service_type: ServiceType::ComputerAgentRunner,
    }
}

fn code(error: shared_types::AppError) -> String {
    match error {
        shared_types::AppError::Structured(detail) => detail.code,
        other => panic!("unexpected unstructured error: {other}"),
    }
}

#[tokio::test]
async fn error_contract_project_query_failure_does_not_create() {
    let runtime = Runtime::new(Scenario::QueryFailed);
    let service: Arc<dyn ContainerRuntime> = runtime.clone();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().to_str().unwrap();
    let result = container_manager::ContainerManager::get_or_create_container(
        container_manager::ContainerCreateOptions {
            project_id: "project",
            service_type: &ServiceType::WebAgentRunner,
            request_resource_limits: None,
            pod_id: Some("isolated-pod"),
            isolation_type: None,
            tenant_id: None,
            space_id: None,
            container_work_path: path,
            runtime: &service,
        },
    )
    .await;
    assert_eq!(
        runtime.creates.load(Ordering::SeqCst),
        0,
        "query error authorized a create"
    );
    assert_eq!(code(result.unwrap_err()), "ERR_RUNTIME_UNAVAILABLE");
}

#[tokio::test]
async fn error_contract_project_true_absence_creates() {
    let runtime = Runtime::new(Scenario::Missing);
    let service: Arc<dyn ContainerRuntime> = runtime.clone();
    let directory = tempfile::tempdir().unwrap();
    let result = container_manager::ContainerManager::get_or_create_container(
        container_manager::ContainerCreateOptions {
            project_id: "project",
            service_type: &ServiceType::WebAgentRunner,
            request_resource_limits: None,
            pod_id: Some("isolated-pod"),
            isolation_type: None,
            tenant_id: None,
            space_id: None,
            container_work_path: directory.path().to_str().unwrap(),
            runtime: &service,
        },
    )
    .await
    .unwrap();
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 1);
    assert_eq!(result.container_id, "captured-container");
}

#[tokio::test]
async fn error_contract_computer_unknown_or_live_unready_is_not_recreated() {
    for (scenario, expected) in [
        (Scenario::QueryFailed, "ERR_RUNTIME_UNAVAILABLE"),
        (
            Scenario::LiveWithoutAddress,
            "ERR_CONTAINER_ADDRESS_NOT_READY",
        ),
        (Scenario::StatusUnknown, "ERR_RUNTIME_UNAVAILABLE"),
    ] {
        let runtime = Runtime::new(scenario);
        let service: Arc<dyn ContainerRuntime> = runtime.clone();
        let result = computer_container_manager::ComputerContainerManager::get_or_create_container_for_user_with_type(&options(), &service).await;
        assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
        assert_eq!(
            runtime.stops.load(Ordering::SeqCst),
            0,
            "live or unknown instance was stopped"
        );
        assert_eq!(code(result.unwrap_err()), expected);
    }
}

#[tokio::test]
async fn error_contract_unconfirmed_cleanup_does_not_create() {
    let runtime = Runtime::new(Scenario::StopUnknown);
    let service: Arc<dyn ContainerRuntime> = runtime.clone();
    let result = computer_container_manager::ComputerContainerManager::get_or_create_container_for_user_with_type(&options(), &service).await;
    assert_eq!(runtime.stops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    assert_eq!(code(result.unwrap_err()), "ERR_OPERATION_OUTCOME_UNKNOWN");
}
