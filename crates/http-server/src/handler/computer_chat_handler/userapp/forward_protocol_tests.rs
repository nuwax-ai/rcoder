//! Admission→生产转发入口→tonic→storage 登记协议回归。
//! builder ensure/文件服务与完整 HTTP 主流程由 Compose/K8s 真实场景验收。
//! 纯 kubernetes 构建的 FQDN 寻址需要真实集群；default 与 deploy-host 在此直拨 fixture。

use super::super::forward::{ComputerForwardParams, forward_computer_request_to_container};
use super::prepare_conversation;
use crate::handler::userapp_agent_target::test_support::{APP_ID, AgentCall, Fixture, USER_ID};
use shared_types::{ComputerChatRequest, ProjectStore as _, UserAppLifecycleStore as _};
use std::time::Duration;

#[tokio::test]
async fn fresh_foreign_hint_reaches_grpc_without_changing_shared_application_route() {
    let fixture = Fixture::new().await;
    let binding_a = fixture.bind("forward-a", "sid-forward-a").await;
    let application = fixture
        .store
        .get_application(APP_ID)
        .await
        .unwrap()
        .unwrap();
    let original_route = fixture.state.projects.get(APP_ID).unwrap();
    let generation = original_route.persistence_identity().generation.clone();
    let container = fixture.runtime.builder.lock().unwrap().clone().unwrap();
    #[cfg(feature = "deploy-host")]
    {
        let observation =
            shared_types::published::begin_physical_observation(&container.container_id);
        assert!(observation.register(
            &[container.container_name.as_str()],
            container.created_at,
            shared_types::published::Reach::Published {
                ports: std::collections::HashMap::from([(
                    shared_types::GRPC_DEFAULT_PORT,
                    shared_types::GRPC_DEFAULT_PORT
                ),])
            },
        ));
    }
    let mut input: ComputerChatRequest = serde_json::from_value(serde_json::json!({
        "user_id": USER_ID, "app_id": APP_ID, "service_type": "userapp",
        "project_id": "forward-b", "session_id": "sid-forward-a", "prompt": "protocol-only",
        "agent_work_dir": "foreign-conversation-directory"
    }))
    .unwrap();
    let (binding, project, fresh) = prepare_conversation(
        &fixture.state,
        APP_ID,
        &application.lifecycle_id,
        &mut input,
    )
    .await
    .unwrap();
    assert!(fresh);
    let result = forward_computer_request_to_container(ComputerForwardParams {
        request: &input,
        project_id: &project,
        work_dir_id: APP_ID,
        container_info: &container,
        grpc_pool: &fixture.state.grpc_pool,
        locale: "zh-CN",
        is_devcomputer: false,
        namespace: &fixture.state.config.app_manager.namespace,
        cluster_domain: &fixture.state.cluster_domain,
        runtime: fixture.state.runtime(),
        service_type: shared_types::ServiceType::UserappBuilder,
        diagnostic_identifier: APP_ID.into(),
        force_new_session: fresh,
    })
    .await;
    assert!(
        result.is_success(),
        "actual gRPC forwarding failed: {result:?}"
    );
    let response = result.data.unwrap();
    assert_eq!(response.project_id, "forward-b");
    assert_ne!(response.session_id, "sid-forward-a");
    let request_id = {
        let calls = fixture.agent.calls.lock().unwrap();
        let AgentCall::Chat(sent) = calls.last().unwrap() else {
            panic!("expected a Chat RPC");
        };
        assert!(sent.force_new_session);
        assert_eq!(sent.project_id, "forward-b");
        assert_eq!(sent.app_id.as_deref(), Some(APP_ID));
        assert_eq!(sent.agent_work_dir.as_deref(), Some(APP_ID));
        assert!(sent.session_id.is_empty());
        sent.request_id.clone().unwrap()
    };
    let registered = fixture
        .state
        .projects
        .register_userapp_agent_session_for_route(
            &binding,
            &response.session_id,
            &request_id,
            tokio::time::Instant::now() + Duration::from_secs(10),
            &generation,
        )
        .await
        .unwrap();
    assert_eq!(registered.project_id, "forward-b");
    assert_eq!(registered.app_id, APP_ID);
    let owner_b = fixture
        .state
        .projects
        .get_userapp_agent_session(&response.session_id)
        .await
        .unwrap()
        .unwrap()
        .0;
    assert_eq!(owner_b, registered);
    let owner_a = fixture
        .state
        .projects
        .get_userapp_agent_session("sid-forward-a")
        .await
        .unwrap()
        .unwrap()
        .0;
    assert_eq!(owner_a, binding_a);
    for sid in ["sid-forward-a", response.session_id.as_str()] {
        let route = fixture.state.projects.get_by_session_id(sid).unwrap();
        assert_eq!(route.project_id(), APP_ID);
        assert_eq!(route.persistence_identity().generation, generation);
    }
    assert!(
        fixture.state.projects.get("forward-b").is_none(),
        "logical conversations must not register extra builder project roots"
    );
    assert_eq!(
        fixture
            .state
            .projects
            .get(APP_ID)
            .unwrap()
            .container_info()
            .map(|info| info.container_id),
        original_route
            .container_info()
            .map(|info| info.container_id)
    );
    assert_eq!(
        fixture
            .agent
            .sessions
            .lock()
            .unwrap()
            .get("forward-a")
            .map(String::as_str),
        Some("sid-forward-a")
    );
    #[cfg(feature = "deploy-host")]
    shared_types::published::unregister_if_physical(
        &container.container_name,
        &container.container_id,
    );
}
