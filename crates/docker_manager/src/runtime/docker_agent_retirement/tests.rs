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
    let snapshot = runtime
        .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
        .await
        .unwrap();
    assert_eq!(snapshot.container.as_ref().unwrap().name, "renamed-owned");
    assert_eq!(
        snapshot.namespace.as_deref(),
        Some(runtime.agent_workload_namespace().as_str())
    );
    assert!(
        runtime.retire_agent_remnants(&snapshot).await.is_err(),
        "Docker automatic writes remain unsupported"
    );
    let result = AgentContainerRuntime::stop_captured_agent(&runtime, &snapshot).await;
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
