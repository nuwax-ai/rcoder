//! Exercise implicit cleanup through the real Bollard HTTP client. The broad
//! legacy prefix also matches persistent applications; only Agent identities
//! may reach inspect/remove.
use super::*;
use serde_json::{Value, json};
use std::sync::Mutex;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[tokio::test]
async fn automatic_cleanup_preserves_persistent_userapps_and_unknown_resources() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(stream.read_u8().await.unwrap());
            }
            let header = String::from_utf8(header).unwrap();
            let request = header.lines().next().unwrap().to_string();
            recorded.lock().unwrap().push(request.clone());
            let body = if request.contains("/containers/json") {
                json!([
                    {"Id":"ordinary-agent", "Names":["/rcoder-old"],
                     "Labels":{"service-type":"web-agent-runner","identifier":"old"}},
                    {"Id":"production-app", "Names":["/rcoder-app-1"],
                     "Labels":{"service-type":"user-app","app-id":"1","managed-by":"rcoder-app-manager"}},
                    {"Id":"persistent-builder", "Names":["/rcoder-app-builder-1"],
                     "Labels":{"service-type":"user-app-builder","identifier":"1"}},
                    {"Id":"unknown", "Names":["/rcoder-unknown"]},
                    {"Id":"foreign", "Names":["/rcoder-foreign"],
                     "Labels":{"service-type":"web-agent-runner","identifier":"foreign","managed-by":"other-controller"}},
                    {"Id":"application-labelled-agent", "Names":["/rcoder-stale"],
                     "Labels":{"service-type":"web-agent-runner","identifier":"stale","rcoder.io/lifecycle-id":"retained-life"}},
                    {"Id":"inconsistent-name", "Names":["/rcoder-other"],
                     "Labels":{"service-type":"web-agent-runner","identifier":"different"}},
                    {"Id":"invalid-family", "Names":["/rcoder-invalid"],
                     "Labels":{"service-type":"unrecognized","identifier":"invalid"}}
                ])
            } else if request.starts_with("GET ")
                && request.contains("/containers/ordinary-agent/json")
            {
                json!({"Id":"ordinary-agent","Name":"/rcoder-old","State":{"Running":true}})
            } else {
                // Record every attempted mutation. A regression that removes a
                // protected container must fail the final request assertion.
                json!({})
            };
            let body = body.to_string();
            stream.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
            ).as_bytes()).await.unwrap();
        }
    });
    let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
    let actor = tokio::spawn(actor.run());
    let manager = Arc::new(DockerManager {
        docker: bollard::Docker::connect_with_http(
            &format!("http://{address}"),
            5,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap(),
        config: crate::DockerManagerConfig::default(),
        containers,
        main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
        api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
    });
    let result = startup_cleanup_containers(&manager, "rcoder-*")
        .await
        .unwrap();
    server.abort();
    actor.abort();
    assert_eq!(result.total_found, 1);
    assert_eq!(result.successfully_removed, 1);
    assert_eq!(result.removed_container_ids, ["ordinary-agent"]);
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        3,
        "protected resources must not be inspected or removed: {requests:?}"
    );
    assert!(requests[0].starts_with("GET ") && requests[0].contains("/containers/json"));
    assert!(
        requests[1].starts_with("GET ") && requests[1].contains("/containers/ordinary-agent/json")
    );
    assert!(
        requests[2].starts_with("DELETE ") && requests[2].contains("/containers/ordinary-agent?")
    );
}

#[test]
fn implicit_cleanup_accepts_only_canonical_ordinary_agent_identities() {
    for service_type in [
        shared_types::ServiceType::WebAgentRunner,
        shared_types::ServiceType::ComputerAgentRunner,
        shared_types::ServiceType::ComputerNormalProject,
    ] {
        let mut candidate: bollard::models::ContainerSummary = serde_json::from_value(json!({
            "Id":"agent", "Names":["/configured-prefix-owner"],
            "Labels":{"service-type":service_type.to_string(),"identifier":"owner"}
        }))
        .unwrap();
        assert!(automatic_agent_cleanup_candidate(
            &candidate,
            "configured-prefix-*"
        ));
        candidate.labels.as_mut().unwrap().insert(
            shared_types::USERAPP_DOCKER_APP_ID_LABEL.into(),
            "owner".into(),
        );
        assert!(!automatic_agent_cleanup_candidate(
            &candidate,
            "configured-prefix-*"
        ));
    }
    for family in [
        "user-app",
        "userapp",
        "UserApp",
        "user-app-builder",
        "userapp-builder",
    ] {
        let candidate: bollard::models::ContainerSummary = serde_json::from_value(json!({
            "Id":"retained", "Names":["/rcoder-owner"],
            "Labels":{"service-type":family,"identifier":"owner"}
        }))
        .unwrap();
        assert!(
            !automatic_agent_cleanup_candidate(&candidate, "rcoder-*"),
            "{family}"
        );
    }
    let unknown: bollard::models::ContainerSummary =
        serde_json::from_value(Value::Object(Default::default())).unwrap();
    assert!(!automatic_agent_cleanup_candidate(&unknown, "rcoder-*"));
}

#[test]
fn implicit_cleanup_patterns_exclude_both_persistent_application_families() {
    let mut config = shared_types::create_default_multi_image_config();
    let ordinary = config.services.values().next().unwrap().clone();
    config.services.clear();
    for service_type in [
        shared_types::ServiceType::Userapp,
        shared_types::ServiceType::UserappBuilder,
    ] {
        let mut service = ordinary.clone();
        service.service_type = service_type;
        service.enabled = true;
        config.services.insert(service_type.to_string(), service);
    }
    assert!(get_container_patterns_for_enabled_services(&config).is_empty());
}
