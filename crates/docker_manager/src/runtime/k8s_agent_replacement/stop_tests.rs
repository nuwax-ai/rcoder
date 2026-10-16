use super::*;
use container_runtime_api::AgentContainerRuntime;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Copy)]
enum Case {
    Completed,
    MismatchedPod,
    ReplacedBeforeDelete,
    Foreign,
    Absent,
    Successor,
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 2048];
    loop {
        let count = stream.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]).into_owned();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                return (headers, bytes[end + 4..end + 4 + length].to_vec());
            }
        }
    }
}

async fn run(case: Case) -> (Result<()>, Vec<(String, serde_json::Value)>) {
    drop(rustls::crypto::ring::default_provider().install_default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = kube::Config::new(format!("http://{address}").parse().unwrap());
    config.default_retry = false;
    let runtime = tests::runtime(kube::Client::try_from(config).unwrap());
    let service_type = ServiceType::ComputerAgentRunner;
    let name = runtime.pod_name("captured-stop", &service_type).unwrap();
    let labels = super::super::k8s_service::build_standard_labels("captured-stop", &service_type);
    let pod = serde_json::json!({"apiVersion":"v1","kind":"Pod","metadata":{
        "name":format!("{name}-0"),"namespace":runtime.namespace,"uid":"captured-pod","resourceVersion":"7","labels":labels,
        "ownerReferences":[{"apiVersion":"apps/v1","kind":"StatefulSet","name":name,"uid":"captured-sts","controller":true}]},
        "status":{"phase":"Failed"}});
    let mut sts = serde_json::json!({"apiVersion":"apps/v1","kind":"StatefulSet","metadata":{
        "name":name,"namespace":runtime.namespace,"uid":"captured-sts","resourceVersion":"8","labels":labels}});
    if matches!(case, Case::Foreign) {
        sts["metadata"]["labels"] = serde_json::json!({});
    }
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let requests = recorded.clone();
    let server = tokio::spawn(async move {
        let mut deleted = false;
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (headers, body) = read_request(&mut stream).await;
            let first = headers.lines().next().unwrap().to_owned();
            let body: serde_json::Value = if body.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::from_slice(&body).unwrap()
            };
            requests.lock().unwrap().push((first.clone(), body.clone()));
            assert!(
                !first.contains("/services") && !first.contains("persistentvolumeclaims"),
                "{first}"
            );
            let path = first
                .split_whitespace()
                .nth(1)
                .unwrap()
                .split('?')
                .next()
                .unwrap();
            let mut code = 200;
            let response = if first.starts_with("DELETE ") {
                assert!(path.ends_with(&format!("/statefulsets/{name}")), "{first}");
                assert_eq!(body["preconditions"]["uid"], "captured-sts");
                assert_eq!(body["preconditions"]["resourceVersion"], "8");
                assert_eq!(body["propagationPolicy"], "Foreground");
                deleted = true;
                if matches!(case, Case::ReplacedBeforeDelete) {
                    code = 409;
                }
                serde_json::json!({"apiVersion":"v1","kind":"Status","code":code,"status":if code == 200 {"Success"} else {"Failure"},"reason":"Conflict","message":"fixture"})
            } else if path.ends_with("/pods") {
                assert!(first.starts_with("GET "));
                let mut items = Vec::new();
                if deleted && matches!(case, Case::Successor) {
                    let mut current = pod.clone();
                    current["metadata"]["uid"] = "successor-pod".into();
                    current["metadata"]["ownerReferences"][0]["uid"] = "successor-sts".into();
                    items.push(current);
                }
                serde_json::json!({"apiVersion":"v1","kind":"PodList","metadata":{},"items":items})
            } else if path.contains("/pods/") {
                assert!(first.starts_with("GET "));
                let mut current = pod.clone();
                if matches!(case, Case::MismatchedPod) {
                    current["metadata"]["uid"] = "successor-pod".into();
                }
                if matches!(case, Case::Absent) {
                    code = 404;
                    serde_json::json!({"apiVersion":"v1","kind":"Status","code":404,"reason":"NotFound","message":"absent"})
                } else {
                    current
                }
            } else if path.contains("/statefulsets/") {
                assert!(first.starts_with("GET "));
                if !deleted {
                    sts.clone()
                } else if matches!(case, Case::Successor) {
                    let mut current = sts.clone();
                    current["metadata"]["uid"] = "successor-sts".into();
                    current
                } else {
                    code = 404;
                    serde_json::json!({"apiVersion":"v1","kind":"Status","code":404,"reason":"NotFound","message":"gone"})
                }
            } else {
                panic!("unexpected stop request {first}")
            };
            let body = response.to_string();
            stream.write_all(format!("HTTP/1.1 {code} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        runtime.stop_agent_container_if_matches("captured-stop", &service_type, "captured-pod"),
    )
    .await
    .unwrap();
    server.abort();
    drop(server.await);
    let requests = recorded.lock().unwrap().clone();
    (result, requests)
}

#[tokio::test]
async fn captured_agent_stop_fences_delete_to_original_sts_and_keeps_services_and_volumes() {
    for case in [Case::Completed, Case::Successor] {
        let (result, requests) = run(case).await;
        result.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|(request, _)| request.starts_with("DELETE "))
                .count(),
            1
        );
        assert_eq!(requests.len(), 5);
    }
}

#[tokio::test]
async fn captured_agent_stop_mismatch_foreign_and_replaced_delete_cannot_target_successor() {
    for case in [
        Case::MismatchedPod,
        Case::Foreign,
        Case::ReplacedBeforeDelete,
    ] {
        let (result, requests) = run(case).await;
        assert!(matches!(result, Err(Error::Conflict(_))), "{result:?}");
        let writes = requests
            .iter()
            .filter(|(request, _)| request.starts_with("DELETE "))
            .count();
        assert_eq!(
            writes,
            usize::from(matches!(case, Case::ReplacedBeforeDelete))
        );
    }
    let (result, requests) = run(Case::Absent).await;
    result.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|(request, _)| request.starts_with("GET "))
    );
}
