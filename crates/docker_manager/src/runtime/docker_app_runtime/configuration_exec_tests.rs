use super::*;

#[test]
fn http_port_label_overrides_docker_tcp_port_observation() {
    let mut ports = vec![AppPortStatus {
        name: String::new(),
        port: 9080,
        expose_type: ExposeType::Tcp,
        external_port: Some(32080),
    }];
    merge_http_port_labels(&mut ports, "9080:http,60000:http,5432:tcp,9999:unknown");
    assert_eq!(ports.len(), 2);
    assert_eq!(ports[0].expose_type, ExposeType::Http);
    assert_eq!(ports[0].external_port, Some(32080));
    assert_eq!(ports[1].port, 60000);
    assert_eq!(ports[1].expose_type, ExposeType::Http);
}

#[test]
fn configuration_exec_requires_physical_lifecycle_and_generation_match() {
    let context = shared_types::UserAppExecutionContext {
        app_id: "app1".into(),
        lifecycle_id: "life1".into(),
        operation_id: "operation1".into(),
        executor_id: "executor1".into(),
        request_fingerprint: "a".repeat(64),
    };
    let target = shared_types::RuntimeConfigurationTarget {
        physical_uid: "container1".into(),
        deployment_generation: "generation1".into(),
    };
    let mut labels = context.resource_metadata();
    labels.insert("managed-by".into(), "rcoder-app-manager".into());
    labels.insert(
        "service-type".into(),
        shared_types::ServiceType::Userapp.to_string(),
    );
    labels.insert(
        shared_types::USERAPP_DOCKER_APP_ID_LABEL.into(),
        "app1".into(),
    );
    let fixture = serde_json::json!({
        "Id":"container1", "Config":{"Labels": labels,
        "Env":[format!("{}=generation1",shared_types::APP_DEPLOY_GENERATION_ID)]}
    });
    let inspected = serde_json::from_value(fixture.clone()).unwrap();
    validate_configuration_exec_target(&context, &target, &inspected).unwrap();
    for (pointer, replacement) in [
        ("/Id", serde_json::json!("container2")),
        (
            "/Config/Labels/rcoder.io~1lifecycle-id",
            serde_json::json!("life2"),
        ),
        (
            "/Config/Labels/service-type",
            serde_json::json!(shared_types::ServiceType::UserappBuilder.to_string()),
        ),
        ("/Config/Env", serde_json::json!([])),
        (
            "/Config/Env",
            serde_json::json!([format!(
                "{}=generation2",
                shared_types::APP_DEPLOY_GENERATION_ID
            )]),
        ),
        (
            "/Config/Env",
            serde_json::json!([
                format!("{}=generation1", shared_types::APP_DEPLOY_GENERATION_ID),
                format!("{}=generation2", shared_types::APP_DEPLOY_GENERATION_ID)
            ]),
        ),
    ] {
        let mut changed = fixture.clone();
        *changed.pointer_mut(pointer).unwrap() = replacement;
        let inspected = serde_json::from_value(changed).unwrap();
        assert!(
            validate_configuration_exec_target(&context, &target, &inspected).is_err(),
            "{pointer}"
        );
    }
}

/// Receipt-free running observation of the captured application container
/// against the real Bollard HTTP client: positive only when the exact
/// captured UID executes on both the inspect and its recheck; a replacement
/// container conflicts; every case stays read-only.
#[tokio::test]
async fn app_running_observation_requires_executing_uid_on_both_reads() {
    use container_runtime_api::UserAppDeploymentRuntime as _;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    for case in [
        "running",
        "stops-during-observation",
        "missing",
        "replacement",
    ] {
        let context = shared_types::UserAppExecutionContext {
            app_id: "51".into(),
            lifecycle_id: "life".into(),
            operation_id: "observe-running".into(),
            executor_id: "worker".into(),
            request_fingerprint: "a".repeat(64),
        };
        let name = app_deployment_name("51");
        let target = shared_types::UserAppMutationTarget {
            context: context.clone(),
            resource: shared_types::AppResourceIdentity {
                kind: shared_types::AppResourceKind::Container,
                name: name.clone(),
                uid: "app-container-id".into(),
                resource_version: None,
            },
        };
        let running_state = serde_json::json!({"Id":"app-container-id",
            "State":{"Running":true,"Status":"running","Restarting":false,"Dead":false}});
        let stopped_state = serde_json::json!({"Id":"app-container-id",
            "State":{"Running":false,"Status":"exited","Restarting":false,"Dead":false}});
        let replaced_state = serde_json::json!({"Id":"replacement-container",
            "State":{"Running":true,"Status":"running","Restarting":false,"Dead":false}});
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let case_owned = case.to_string();
        let expected_name = name.clone();
        let server = tokio::spawn(async move {
            for read in 1..=2u32 {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let count = stream.read(&mut buffer).await.expect("read");
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                        break;
                    }
                }
                let headers = String::from_utf8_lossy(&bytes);
                let request = headers.lines().next().expect("request").to_owned();
                assert!(
                    request.starts_with("GET ")
                        && request.contains(&format!("/containers/{expected_name}/json")),
                    "read-only exact inspect: {request}"
                );
                let (status, body) = match case_owned.as_str() {
                    "missing" => (
                        404,
                        serde_json::json!({"message":"no such container"}).to_string(),
                    ),
                    "replacement" => (200, replaced_state.to_string()),
                    "stops-during-observation" => (
                        200,
                        if read == 1 {
                            running_state.to_string()
                        } else {
                            stopped_state.to_string()
                        },
                    ),
                    _ => (200, running_state.to_string()),
                };
                stream.write_all(format!("HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.expect("reply");
            }
        });
        let (_actor, containers) = crate::container_state_actor::ContainerStateActor::new();
        let manager = Arc::new(crate::DockerManager {
            docker: bollard::Docker::connect_with_http(
                &format!("http://{address}"),
                1,
                bollard::API_DEFAULT_VERSION,
            )
            .expect("Docker client"),
            config: crate::DockerManagerConfig::default(),
            containers,
            main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
            api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
        });
        let runtime = DockerRuntime::new(manager);
        let observed = runtime.observe_app_compute_running(&target).await;
        match case {
            "running" => assert!(observed.expect("executing uid on both reads"), "{case}"),
            "replacement" => assert!(
                matches!(observed, Err(ContainerRuntimeError::Conflict(_))),
                "{case}: {observed:?}"
            ),
            _ => assert!(
                !observed.expect("absent or stopped shapes are not running"),
                "{case}"
            ),
        }
        server.abort();
    }
}
