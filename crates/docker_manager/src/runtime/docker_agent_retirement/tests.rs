use super::*;
use container_runtime_api::AgentContainerRuntime;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy)]
enum Case {
    Removed,
    Unknown,
    Foreign,
}

async fn run(case: Case) -> (Result<AgentStopOutcome>, Vec<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let requests = recorded.clone();
    let server = tokio::spawn(async move {
        let mut removed = false;
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                bytes.push(stream.read_u8().await.unwrap());
            }
            let first = String::from_utf8(bytes)
                .unwrap()
                .lines()
                .next()
                .unwrap()
                .to_owned();
            requests.lock().unwrap().push(first.clone());
            let mut code = 200;
            let response = if first.contains("/containers/json") {
                assert!(
                    first.contains("all=true"),
                    "stopped/dead containers must be observed"
                );
                serde_json::json!([{"Id":"captured-id","Names":["/renamed-owned"],"Created":1,"State":"exited","Labels":{"identifier":"owner","service-type":"computer-agent-runner"}}])
            } else if first.starts_with("DELETE ") {
                assert!(first.contains("/containers/captured-id?"));
                assert!(first.contains("v=false"));
                removed = true;
                code = 204;
                serde_json::Value::Null
            } else {
                assert!(first.contains("/containers/captured-id/json"));
                if removed {
                    code = if matches!(case, Case::Unknown) {
                        500
                    } else {
                        404
                    };
                    serde_json::json!({"message":"confirmation failed"})
                } else {
                    serde_json::json!({"Id":"captured-id","Name":"/renamed-owned","Config":{"Labels":{"identifier":if matches!(case,Case::Foreign){"foreign"}else{"owner"},"service-type":"computer-agent-runner"}},"State":{"Running":false,"Status":"exited"}})
                }
            };
            let body = if code == 204 {
                String::new()
            } else {
                response.to_string()
            };
            stream.write_all(format!("HTTP/1.1 {code} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
    tokio::spawn(actor.run());
    let manager = Arc::new(crate::DockerManager {
        docker: bollard::Docker::connect_with_http(
            &format!("http://{address}"),
            5,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap(),
        config: Default::default(),
        containers,
        main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
        api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
    });
    let runtime = DockerRuntime::new(manager);
    let result = match runtime
        .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
        .await
    {
        Ok(snapshot) => {
            assert_eq!(snapshot.container.as_ref().unwrap().name, "renamed-owned");
            assert_eq!(
                snapshot.namespace.as_deref(),
                Some(runtime.agent_workload_namespace().as_str())
            );
            AgentContainerRuntime::stop_captured_agent(&runtime, &snapshot).await
        }
        Err(error) => Err(error),
    };
    server.abort();
    drop(server.await);
    let requests = recorded.lock().unwrap().clone();
    (result, requests)
}

#[tokio::test]
async fn captured_docker_stop_uses_full_id_after_rename_and_preserves_volumes() {
    let (result, requests) = run(Case::Removed).await;
    assert_eq!(result.unwrap(), AgentStopOutcome::Complete);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("DELETE "))
            .count(),
        1
    );
}

#[tokio::test]
async fn captured_docker_stop_rejects_foreign_and_preserves_unknown_confirmation() {
    let (result, requests) = run(Case::Foreign).await;
    assert!(result.is_err());
    assert!(
        !requests
            .iter()
            .any(|request| request.starts_with("DELETE "))
    );
    let (result, requests) = run(Case::Unknown).await;
    assert!(result.is_err());
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("DELETE "))
            .count(),
        1
    );
}

#[test]
fn unrelated_mount_does_not_protect_workspace() {
    let inspect = serde_json::from_value(serde_json::json!({
        "Mounts": [{"Type":"bind", "Source":"/var/run/docker.sock", "Destination":"/var/run/docker.sock", "RW":true}]
    })).unwrap();
    assert!(!workspace_is_persistent(
        &inspect,
        "/managed/project",
        "/app/project_workspace/project"
    ));
}

#[test]
fn other_deployment_workspace_is_not_our_retirement_authority() {
    let inspect = serde_json::from_value(serde_json::json!({
        "Mounts": [{"Type":"bind", "Source":"/other/project", "Destination":"/app/project_workspace/project", "RW":true}]
    })).unwrap();
    assert!(!workspace_is_persistent(
        &inspect,
        "/managed/project",
        "/app/project_workspace/project"
    ));
}

#[test]
fn same_cid_restart_invalidates_automatic_delete_witness_at_runtime() {
    let mut snapshot = AgentStopSnapshot {
        namespace: Some("docker:test".into()),
        identifier: "project".into(),
        service_type: ServiceType::WebAgentRunner,
        app_id_hint: None,
        controller: None,
        pods: vec![],
        container: Some(AgentResourceIdentity {
            kind: AgentResourceKind::Container,
            name: "rcoder-project".into(),
            uid: "full-cid".into(),
            resource_version: Some("before".into()),
            created_at: None,
            owner_uid: None,
        }),
        services: vec![],
        replicas: None,
        endpoint: None,
        compute_data_protection: Some(ComputeDataProtection {
            persistent_mounts: true,
            auto_remove: false,
        }),
    };
    let inspect =
        serde_json::from_value(serde_json::json!({"State":{"StartedAt":"after"}})).unwrap();
    assert!(validate_automatic_evidence(&snapshot, &inspect).is_err());
    snapshot.container.as_mut().unwrap().resource_version = Some("after".into());
    assert!(validate_automatic_evidence(&snapshot, &inspect).is_ok());
    snapshot.service_type = ServiceType::UserappBuilder;
    assert!(validate_automatic_evidence(&snapshot, &inspect).is_err());
}

#[test]
fn persistent_workspace_requires_exact_source_destination_and_no_shadow_mount() {
    let mut inspect: bollard::models::ContainerInspectResponse = serde_json::from_value(serde_json::json!({
        "Mounts": [{"Type":"bind", "Source":"/managed/project", "Destination":"/app/project_workspace/project", "RW":true}]
    })).unwrap();
    assert!(workspace_is_persistent(
        &inspect,
        "/managed/project",
        "/app/project_workspace/project"
    ));
    inspect.mounts.as_mut().unwrap().push(
        serde_json::from_value(serde_json::json!({
            "Type":"tmpfs", "Destination":"/app/project_workspace/project/data", "RW":true
        }))
        .unwrap(),
    );
    assert!(!workspace_is_persistent(
        &inspect,
        "/managed/project",
        "/app/project_workspace/project"
    ));
    inspect.mounts.as_mut().unwrap().pop();
    inspect.mounts.as_mut().unwrap()[0].rw = None;
    assert!(!workspace_is_persistent(
        &inspect,
        "/managed/project",
        "/app/project_workspace/project"
    ));
}

#[test]
fn owned_web_workspace_is_eligible_and_shared_workloads_use_creation_scope() {
    let config = shared_types::default_rcoder_service_config();
    let mut inspect: bollard::models::ContainerInspectResponse = serde_json::from_value(serde_json::json!({
        "Config":{"Labels":{"identifier":"project","service-type":"web-agent-runner"},
            "Env":["PROJECT_ID=project","PROJECT_WORKSPACE_BASE=/app/project_workspace"]},
        "Mounts":[{"Type":"bind","Source":"/managed/project","Destination":"/app/project_workspace/project","RW":true}]
    })).unwrap();
    let owned = |details: &_| {
        persistent_agent_workspace_in_scope(
            details,
            ServiceType::WebAgentRunner,
            "project",
            &config,
            std::path::Path::new("/managed"),
        )
    };
    assert!(owned(&inspect));
    assert!(!persistent_agent_workspace_in_scope(
        &inspect,
        ServiceType::WebAgentRunner,
        "shared",
        &config,
        std::path::Path::new("/managed")
    ));
    inspect
        .config
        .as_mut()
        .unwrap()
        .env
        .as_mut()
        .unwrap()
        .extend([
            "TENANT_ID=tenant".into(),
            "SPACE_ID=space".into(),
            "ISOLATION_TYPE=space".into(),
        ]);
    let mount = &mut inspect.mounts.as_mut().unwrap()[0];
    mount.source = Some("/managed/tenant/space".into());
    mount.destination = Some("/app/project_workspace/tenant/space".into());
    assert!(persistent_agent_workspace_in_scope(
        &inspect,
        ServiceType::WebAgentRunner,
        "shared",
        &config,
        std::path::Path::new("/managed")
    ));
    inspect
        .config
        .as_mut()
        .unwrap()
        .labels
        .as_mut()
        .unwrap()
        .insert("managed-by".into(), "another-manager".into());
    assert!(!persistent_agent_workspace_in_scope(
        &inspect,
        ServiceType::WebAgentRunner,
        "shared",
        &config,
        std::path::Path::new("/managed")
    ));
}
