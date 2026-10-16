//! Central registration + real memory storage protocol tests. The runtime proof
//! is a controlled trait fixture, not evidence from a live Kubernetes API.
use super::*;
use async_trait::async_trait;
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
    RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use shared_types::{
    AgentWorkspaceIdentity, VerifiedAgentReplacement, persistence::PersistenceWriteOutcome,
};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

type CapturedVerification = (Option<String>, Option<String>, Option<String>);

struct ReplacementRuntime {
    original: ContainerBasicInfo,
    fresh: ContainerBasicInfo,
    verifications: Mutex<Vec<CapturedVerification>>,
    writes: AtomicUsize,
}
#[async_trait]
impl AgentContainerRuntime for ReplacementRuntime {
    async fn verify_agent_replacement(
        &self,
        params: &ContainerCreateParams,
        previous: Option<&ContainerBasicInfo>,
        replacement: &ContainerBasicInfo,
    ) -> ContainerRuntimeResult<VerifiedAgentReplacement> {
        let identifier = params
            .service_type
            .container_identifier(
                params.pod_id.as_deref(),
                params.user_id.as_deref(),
                params.project_id.as_deref(),
            )
            .map_err(|error| ContainerRuntimeError::ConfigurationError(error.to_string()))?;
        assert_eq!(identifier, self.original.project_id);
        assert_eq!(replacement.container_id, self.fresh.container_id);
        if let Some(previous) = previous {
            assert_eq!(
                previous.container_id, self.original.container_id,
                "the canonical predecessor must be frozen before registration changes"
            );
            assert_eq!(previous.workload_uid, self.original.workload_uid);
        }
        self.verifications.lock().unwrap().push((
            previous.map(|basic| basic.container_id.clone()),
            params.user_id.clone(),
            params.pod_id.clone(),
        ));
        Ok(VerifiedAgentReplacement {
            identifier: identifier.into(),
            service_type: params.service_type,
            previous: previous.cloned(),
            replacement: self.fresh.clone(),
            workspace: AgentWorkspaceIdentity {
                namespace: "isolated-protocol-fixture".into(),
                claim_name: "retained-workspace".into(),
                claim_uid: "same-retained-pvc".into(),
                mount_path: "/home/user".into(),
                sub_path: None,
            },
        })
    }
    async fn create_container(
        &self,
        _: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Err(ContainerRuntimeError::ConfigurationError(
            "unexpected create".into(),
        ))
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
        self.writes.fetch_add(1, Ordering::SeqCst);
        Err(ContainerRuntimeError::ConfigurationError(
            "unexpected stop".into(),
        ))
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        Ok(false)
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
impl WorkspaceRuntime for ReplacementRuntime {}
#[async_trait]
impl UserAppDeploymentRuntime for ReplacementRuntime {}

fn project(
    id: &str,
    user: &str,
    pod: Option<&str>,
    basic: &ContainerBasicInfo,
) -> ProjectAndContainerInfo {
    let mut info = ProjectAndContainerInfo::new(id.into());
    info.set_user_id(Some(user.into()));
    info.set_pod_id(pod.map(str::to_owned));
    if pod.is_some() {
        info.set_scope(
            Some("same-tenant".into()),
            Some("same-space".into()),
            Some("tenant".into()),
        );
    }
    info.set_service_type(Some(ServiceType::ComputerAgentRunner));
    info.set_container(Some(basic.clone()));
    info
}

async fn new_project_replacement(first_user: &str, second_user: &str, pod: Option<&str>) {
    use crate::app_state::RegistrationMetadata;
    let fixture = Fixture::new().await;
    let identifier = pod.unwrap_or(first_user);
    let mut original = basic();
    original.project_id = identifier.into();
    original.container_name = "same-canonical-workload".into();
    original.workload_uid = Some("original-controller".into());
    fixture
        .state
        .insert_project_with_session(
            "canonical-a".into(),
            Arc::new(project("canonical-a", first_user, pod, &original)),
            "a-historical-session",
        )
        .unwrap();
    let mut dispatched_old_a = (*fixture.state.get_project("canonical-a").unwrap()).clone();
    dispatched_old_a.set_container(Some(original.clone()));
    let mut fresh = original.clone();
    fresh.container_id = "replacement-pod".into();
    fresh.workload_uid = Some("replacement-controller".into());
    fresh.container_ip = "127.0.0.2".into();
    fresh.service_url = "http://127.0.0.2:50051".into();
    let runtime = Arc::new(ReplacementRuntime {
        original: original.clone(),
        fresh: fresh.clone(),
        verifications: Mutex::new(Vec::new()),
        writes: AtomicUsize::new(0),
    });
    let mut state = (*fixture.state).clone();
    state.runtime = runtime.clone();
    let state = Arc::new(state);
    assert!(state.get_project("canonical-b").is_none());
    let options = ContainerCreateOptions {
        user_id: second_user.into(),
        project_id: "canonical-b".into(),
        resource_limits: None,
        pod_id: pod.map(str::to_owned),
        isolation_type: pod.map(|_| "tenant".into()),
        tenant_id: pod.map(|_| "same-tenant".into()),
        space_id: pod.map(|_| "same-space".into()),
        service_type: ServiceType::ComputerAgentRunner,
    };
    let params = ComputerContainerManager::creation_params(&options).unwrap();
    let registered_b = state.ensure_computer_project_mapping(&params,
        Arc::new(project("canonical-b", second_user, pod, &fresh)), RegistrationMetadata::UpdateChatMetadata)
        .await.expect("a new project must recover the owned canonical workload without requiring old A to run first");
    let registered_a = state.get_project("canonical-a").unwrap();
    for info in [&registered_a, &registered_b] {
        let basic = info.container_info().unwrap();
        assert_eq!(basic.container_id, fresh.container_id);
        assert_eq!(basic.workload_uid, fresh.workload_uid);
        assert_eq!(
            basic.container_ip, fresh.container_ip,
            "dispatch must use the verified fresh address"
        );
        assert_eq!(basic.project_id, identifier);
    }
    assert_eq!(registered_a.user_id(), Some(first_user));
    assert_eq!(registered_b.user_id(), Some(second_user));
    assert_eq!(registered_a.pod_id(), pod);
    assert_eq!(registered_b.pod_id(), pod);
    assert!(registered_a.sessions().contains("a-historical-session"));
    assert!(
        runtime
            .verifications
            .lock()
            .unwrap()
            .iter()
            .any(|(previous, _, _)| previous.as_deref() == Some(original.container_id.as_str())),
        "canonical predecessor must have fresh runtime verification"
    );
    assert_eq!(
        runtime.writes.load(Ordering::SeqCst),
        0,
        "re-registration cannot mutate compute or volumes"
    );
    assert_eq!(
        state
            .append_session_for_container_durable(&dispatched_old_a, "late-old-a-completion")
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    assert!(
        state
            .get_by_session_with_fetch_result("late-old-a-completion")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        state
            .get_by_session_with_fetch_result("a-historical-session")
            .await
            .unwrap()
            .is_some()
    );
    let mut dispatched_b = (*registered_b).clone();
    dispatched_b.set_container(Some(fresh));
    assert_eq!(
        state
            .append_session_for_container_durable(&dispatched_b, "new-b-session")
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert_eq!(
        state
            .get_by_session_with_fetch_result("new-b-session")
            .await
            .unwrap()
            .unwrap()
            .project_id(),
        "canonical-b"
    );
}

#[tokio::test]
async fn new_project_recovers_same_user_canonical_replacement_and_fences_old_completion() {
    new_project_replacement("same-user", "same-user", None).await;
}

#[tokio::test]
async fn new_project_in_explicit_shared_pod_keeps_distinct_users_while_recovering_workload() {
    new_project_replacement("shared-user-a", "shared-user-b", Some("shared-tenant-pod")).await;
}
