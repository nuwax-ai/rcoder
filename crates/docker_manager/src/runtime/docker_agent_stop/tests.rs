use super::*;
use container_runtime_api::AgentContainerRuntime;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy)]
enum Case {
    Captured,
    MismatchedId,
    Foreign,
    Absent,
}

async fn run(case: Case) -> (Result<()>, Vec<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let requests = recorded.clone();
    let service_type = ServiceType::ComputerAgentRunner;
    let name = crate::utils::DockerUtils::generate_container_name(
        service_type.container_prefix(),
        "captured-stop",
    )
    .unwrap();
    let server = tokio::spawn(async move {
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
            assert!(
                first.contains("/containers/captured-id"),
                "must address captured ID, not name: {first}"
            );
            requests.lock().unwrap().push(first.clone());
            if first.starts_with("DELETE ") {
                stream.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            } else {
                assert!(first.starts_with("GET "));
                let (code, body) = if matches!(case, Case::Absent) {
                    (404, serde_json::json!({"message":"captured ID absent"}))
                } else {
                    (
                        200,
                        serde_json::json!({"Id":if matches!(case, Case::MismatchedId) {"successor-id"} else {"captured-id"},
                        "Name":format!("/{name}"),"Config":{"Labels":{"service-type":service_type.to_string(),
                            "identifier":if matches!(case, Case::Foreign) {"another-owner"} else {"captured-stop"}}},
                        "State":{"Running":false,"Status":"exited"}}),
                    )
                };
                let body = body.to_string();
                stream.write_all(format!("HTTP/1.1 {code} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
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
    let successor = crate::DockerContainerInfo::new(
        "successor-id".into(),
        "successor-name".into(),
        "captured-stop".into(),
        "image".into(),
    );
    manager
        .containers
        .insert("captured-stop".into(), successor)
        .await;
    let runtime = DockerRuntime::new(manager.clone());
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        runtime.stop_agent_container_if_matches("captured-stop", &service_type, "captured-id"),
    )
    .await
    .unwrap();
    assert_eq!(
        manager
            .containers
            .get("captured-stop")
            .await
            .unwrap()
            .container_id,
        "successor-id"
    );
    server.abort();
    drop(server.await);
    let requests = recorded.lock().unwrap().clone();
    (result, requests)
}

#[tokio::test]
async fn captured_agent_stop_docker_deletes_only_captured_id_and_preserves_successor_cache() {
    let (result, requests) = run(Case::Captured).await;
    result.unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("DELETE "))
            .count(),
        1
    );
}

#[tokio::test]
async fn captured_agent_stop_docker_rejects_mismatch_and_foreign_without_deletion() {
    for case in [Case::MismatchedId, Case::Foreign] {
        let (result, requests) = run(case).await;
        assert!(matches!(result, Err(Error::Conflict(_))), "{result:?}");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("GET "));
    }
    let (result, requests) = run(Case::Absent).await;
    result.unwrap();
    assert_eq!(requests.len(), 1);
}
