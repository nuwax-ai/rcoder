//! Physical Docker stopped observation, using the real Bollard HTTP client.
use super::*;
use container_runtime_api::AgentContainerRuntime as _;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn stopped_observation_requires_exact_retained_inactive_container_without_a_stop_receipt() {
    for case in [
        "exited",
        "running",
        "created",
        "unknown",
        "restarting",
        "dead",
        "replacement",
        "foreign-app",
        "foreign-lifecycle",
        "missing",
        "read-error",
    ] {
        let context = UserAppExecutionContext {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            operation_id: "observe-external-stop".into(),
            executor_id: "worker".into(),
            request_fingerprint: "a".repeat(64),
        };
        let mut labels = context.resource_metadata();
        labels.insert(
            "service-type".into(),
            ServiceType::UserappBuilder.to_string(),
        );
        labels.insert("identifier".into(), "app".into());
        let binding = shared_types::UserAppResourceBinding {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            service_type: ServiceType::UserappBuilder,
            physical_uid: "original-id".into(),
            adopted_by_operation: "adopt".into(),
        };
        let initial: bollard::models::ContainerInspectResponse = serde_json::from_value(
            serde_json::json!({"Id":"original-id","Config":{"Labels":labels}}),
        )
        .expect("initial inspect");
        let target = BuilderControlTarget {
            resource_binding: Some(binding.clone()),
            context: context.clone(),
            workload: Some(
                control_identity_with_binding(
                    &initial,
                    "builder-app",
                    &context,
                    Some(&binding),
                    false,
                )
                .expect("captured identity"),
            ),
            pod: None,
            restart_image: None,
            restart_runtime_workspace: None,
            builder_startup_mode: shared_types::BuilderStartupMode::default(),
        };
        let mut response = serde_json::json!({"Id":"original-id","Config":{"Labels":labels},
            "State":{"Running":false,"Status":"exited","Restarting":false,"Dead":false}});
        match case {
            "exited" | "missing" | "read-error" => {}
            "running" => {
                response["State"]["Running"] = true.into();
                response["State"]["Status"] = "running".into();
            }
            "created" => response["State"]["Status"] = "created".into(),
            "unknown" => response["State"]["Status"] = serde_json::Value::Null,
            "restarting" => response["State"]["Restarting"] = true.into(),
            "dead" => response["State"]["Dead"] = true.into(),
            "replacement" => response["Id"] = "new-container".into(),
            "foreign-app" => response["Config"]["Labels"]["identifier"] = "another-app".into(),
            "foreign-lifecycle" => {
                response["Config"]["Labels"]["rcoder.io/lifecycle-id"] = "another-life".into()
            }
            _ => unreachable!("enumerated case"),
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorded = requests.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = stream.read(&mut buffer).await.expect("read");
                assert!(count > 0, "inspect must send its HTTP request");
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&bytes);
            let request = headers.lines().next().expect("request").to_owned();
            assert!(
                request.starts_with("GET ") && request.contains("/containers/original-id/json"),
                "read-only exact inspect: {request}"
            );
            recorded.lock().expect("recorded").push(request);
            let status = if case == "missing" {
                404
            } else if case == "read-error" {
                403
            } else {
                200
            };
            let body = if status == 200 {
                response.to_string()
            } else {
                serde_json::json!({"message":"fixture observation error"}).to_string()
            };
            stream.write_all(format!("HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.expect("reply");
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
        let observed = runtime.observe_builder_compute_stopped(&target).await;
        match case {
            "exited" | "created" => assert!(
                observed.expect("retained inactive compute"),
                "{case} has no executing business process and must enter controlled management wake"
            ),
            "replacement" | "foreign-app" | "foreign-lifecycle" => assert!(
                matches!(observed, Err(Error::Conflict(_))),
                "{case}: {observed:?}"
            ),
            "read-error" => assert!(
                matches!(observed, Err(Error::DockerError(_))),
                "read errors stay errors: {observed:?}"
            ),
            _ => assert!(!observed.expect("still not confirmed stopped"), "{case}"),
        }
        assert_eq!(
            requests.lock().expect("requests").len(),
            1,
            "one actual inspect without starts/stops or local receipts"
        );
        server.await.expect("read-only server");
    }
}

/// Positive running observation (deadline-settlement probe) against the real
/// Bollard HTTP client: positive only when the exact captured identity is
/// executing on BOTH the inspect and its recheck. The mid-observation stop
/// case proves the recheck is real; replacements conflict, errors stay
/// errors, and every case stays read-only.
#[tokio::test]
async fn running_observation_requires_executing_identity_on_both_reads() {
    for case in [
        "running",
        "stops-during-observation",
        "exited",
        "missing",
        "replacement",
        "read-error",
    ] {
        let context = UserAppExecutionContext {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            operation_id: "observe-running".into(),
            executor_id: "worker".into(),
            request_fingerprint: "a".repeat(64),
        };
        let mut labels = context.resource_metadata();
        labels.insert(
            "service-type".into(),
            ServiceType::UserappBuilder.to_string(),
        );
        labels.insert("identifier".into(), "app".into());
        let binding = shared_types::UserAppResourceBinding {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            service_type: ServiceType::UserappBuilder,
            physical_uid: "original-id".into(),
            adopted_by_operation: "adopt".into(),
        };
        let initial: bollard::models::ContainerInspectResponse = serde_json::from_value(
            serde_json::json!({"Id":"original-id","Config":{"Labels":labels}}),
        )
        .expect("initial inspect");
        let target = BuilderControlTarget {
            resource_binding: Some(binding),
            context: context.clone(),
            workload: Some(
                control_identity_with_binding(&initial, "builder-app", &context, None, false)
                    .expect("captured identity"),
            ),
            pod: None,
            restart_image: None,
            restart_runtime_workspace: None,
            builder_startup_mode: shared_types::BuilderStartupMode::default(),
        };
        let running_state = serde_json::json!({"Id":"original-id","Config":{"Labels":labels},
            "State":{"Running":true,"Status":"running","Restarting":false,"Dead":false}});
        let stopped_state = serde_json::json!({"Id":"original-id","Config":{"Labels":labels},
            "State":{"Running":false,"Status":"exited","Restarting":false,"Dead":false}});
        let replaced_state = serde_json::json!({
            "Id":"new-container","Config":{"Labels":labels},
            "State":{"Running":true,"Status":"running","Restarting":false,"Dead":false}});
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let recorded = requests.clone();
        let case_owned = case.to_string();
        let server = tokio::spawn(async move {
            for read in 1..=2u32 {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let count = stream.read(&mut buffer).await.expect("read");
                    assert!(count > 0, "inspect must send its HTTP request");
                    bytes.extend_from_slice(&buffer[..count]);
                    if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                        break;
                    }
                }
                let headers = String::from_utf8_lossy(&bytes);
                let request = headers.lines().next().expect("request").to_owned();
                assert!(
                    request.starts_with("GET ") && request.contains("/containers/original-id/json"),
                    "read-only exact inspect: {request}"
                );
                recorded.lock().expect("recorded").push(request);
                let (status, body) = match case_owned.as_str() {
                    "missing" => (
                        404,
                        serde_json::json!({"message":"no such container"}).to_string(),
                    ),
                    "read-error" => (
                        403,
                        serde_json::json!({"message":"fixture error"}).to_string(),
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
                    "running" => (200, running_state.to_string()),
                    _ => (200, stopped_state.to_string()),
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
        let observed = runtime.observe_builder_compute_running(&target).await;
        match case {
            "running" => assert!(
                observed.expect("executing identity on both reads"),
                "{case}"
            ),
            "replacement" => assert!(
                matches!(observed, Err(Error::Conflict(_))),
                "{case}: {observed:?}"
            ),
            "read-error" => assert!(
                matches!(observed, Err(Error::DockerError(_))),
                "read errors stay errors: {observed:?}"
            ),
            "stops-during-observation" => assert!(
                !observed.expect("recheck sees the stop"),
                "the mid-observation stop must be honored by the recheck"
            ),
            _ => assert!(
                !observed.expect("absent or exited shapes are not running"),
                "{case}"
            ),
        }
        assert!(
            requests
                .lock()
                .expect("recorded")
                .iter()
                .all(|r| r.starts_with("GET ")),
            "running observation is read-only: {case}"
        );
        server.abort();
    }
}
