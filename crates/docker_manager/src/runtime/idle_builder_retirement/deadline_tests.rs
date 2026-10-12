//! Actual HTTP clients must share one deadline, including reads and post-write
//! confirmation. The fixture retains late server activity without client retry.
use super::tests::{captured, reply, request};
use super::*;
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn idle_docker_shared_deadline_covers_reads_and_unknown_writes_without_cache_retirement() {
    for phase in ["read", "remove", "confirm"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let server = tokio::spawn(async move {
            let mut removed = false;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (first, _) = request(&mut stream).await;
                recorded.lock().unwrap().push(first.clone());
                assert!(
                    first.contains("/containers/old-id/") || first.contains("/containers/old-id?")
                );
                let stall = match phase {
                    "read" => first.starts_with("GET "),
                    "remove" => first.starts_with("DELETE "),
                    "confirm" => removed && first.starts_with("GET "),
                    _ => unreachable!(),
                };
                if stall {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    // The original implementation only returns after this
                    // connection closes. The fixed future must finish earlier.
                    break;
                }
                if first.contains("/exec") {
                    reply(
                        &mut stream,
                        400,
                        serde_json::json!({"message":"owner unavailable"}),
                    )
                    .await;
                } else if first.starts_with("DELETE ") {
                    removed = true;
                    assert!(first.contains("v=false"));
                    reply(&mut stream, 200, serde_json::json!({})).await;
                } else {
                    reply(&mut stream,200,serde_json::json!({"Id":"old-id","Name":"/rcoder-app-builder-review","Config":{"Labels":{"service-type":"user-app-builder","identifier":"review"}}})).await;
                }
            }
        });
        let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
        let actor_task = tokio::spawn(actor.run());
        containers
            .insert(
                "review".into(),
                crate::DockerContainerInfo::new(
                    "old-id".into(),
                    "rcoder-app-builder-review".into(),
                    "review".into(),
                    "immutable:old".into(),
                ),
            )
            .await;
        let runtime = DockerRuntime::new(Arc::new(crate::DockerManager {
            docker: bollard::Docker::connect_with_http(
                &format!("http://{address}"),
                120,
                bollard::API_DEFAULT_VERSION,
            )
            .unwrap(),
            config: crate::DockerManagerConfig::default(),
            containers: containers.clone(),
            main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
            api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
        }));
        let began = tokio::time::Instant::now();
        let result = runtime
            .retire_captured_idle_builder_until(
                "review",
                &captured(None),
                began + std::time::Duration::from_millis(50),
            )
            .await;
        let elapsed = began.elapsed();
        let cache = containers.get("review").await;
        actor_task.abort();
        server.await.unwrap();
        assert!(
            elapsed < std::time::Duration::from_millis(150),
            "{phase} escaped shared deadline: {elapsed:?}, {result:?}"
        );
        let Error::Timeout(message) = result.unwrap_err() else {
            panic!("shared deadline must classify timeout")
        };
        assert_eq!(message.contains("unknown"), phase != "read");
        assert_eq!(
            cache.unwrap().container_id,
            "old-id",
            "no unconfirmed cache retirement"
        );
        let actual = requests.lock().unwrap();
        if phase == "read" {
            assert_eq!(actual.len(), 1);
            assert!(actual[0].starts_with("GET "));
        } else {
            assert_eq!(
                actual
                    .iter()
                    .filter(|row| row.starts_with("DELETE "))
                    .count(),
                1
            );
        }
    }
}

#[cfg(feature = "kubernetes")]
#[tokio::test]
async fn idle_kubernetes_shared_deadline_covers_reads_and_unknown_writes_without_retargeting() {
    drop(rustls::crypto::ring::default_provider().install_default());
    for phase in ["read", "delete", "confirm"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let server = tokio::spawn(async move {
            let mut removed = false;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (first, body) = request(&mut stream).await;
                recorded.lock().unwrap().push(first.clone());
                assert!(!first.contains("persistentvolume"));
                let path = first.split_whitespace().nth(1).unwrap();
                let name = path.split('?').next().unwrap().rsplit('/').next().unwrap();
                let stall = match phase {
                    "read" => first.starts_with("GET ") && path.contains("/statefulsets/"),
                    "delete" => first.starts_with("DELETE "),
                    "confirm" => removed && path.contains("/pods/"),
                    _ => unreachable!(),
                };
                if stall {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    break;
                }
                if path.contains("/exec?") {
                    reply(&mut stream,400,serde_json::json!({"kind":"Status","apiVersion":"v1","code":400,"reason":"BadRequest","message":"owner unavailable"})).await;
                } else if first.starts_with("DELETE ") {
                    let params: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(params["preconditions"]["resourceVersion"], "7");
                    let uid = if path.contains("/statefulsets/") {
                        "old-workload".to_owned()
                    } else {
                        format!("old-{name}")
                    };
                    assert_eq!(params["preconditions"]["uid"], uid);
                    removed = true;
                    reply(
                        &mut stream,
                        200,
                        serde_json::json!({"kind":"Status","apiVersion":"v1","status":"Success"}),
                    )
                    .await;
                } else {
                    let kind = if path.contains("/pods/") {
                        "Pod"
                    } else if path.contains("/statefulsets/") {
                        "StatefulSet"
                    } else {
                        "Service"
                    };
                    let uid = if kind == "Pod" {
                        "old-id".to_owned()
                    } else if kind == "StatefulSet" {
                        "old-workload".to_owned()
                    } else {
                        format!("old-{name}")
                    };
                    reply(&mut stream,200,serde_json::json!({"kind":kind,"apiVersion":if kind=="StatefulSet" {"apps/v1"} else {"v1"},
                        "metadata":{"name":name,"uid":uid,"resourceVersion":"7","labels":{"rcoder.io/service-type":"user-app-builder","rcoder.io/identifier":"review"},
                        "ownerReferences":if kind=="Pod" {serde_json::json!([{"apiVersion":"apps/v1","kind":"StatefulSet","name":"rcoder-app-builder-review","uid":"old-workload","controller":true}])} else {serde_json::json!([])}},
                        "spec":if kind=="Service" {serde_json::json!({"clusterIP":if name.ends_with("-headless") {"None"} else {"10.0.0.1"},"selector":{"app.kubernetes.io/name":"user-app-builder","app.kubernetes.io/instance":"review","app.kubernetes.io/managed-by":"rcoder-runtime","rcoder.io/identifier":"review"}})} else {serde_json::json!({})}})).await;
                }
            }
        });
        let client = kube::Client::try_from(kube::Config::new(
            format!("http://{address}").parse().unwrap(),
        ))
        .unwrap();
        let runtime = tests::kubernetes_runtime(client);
        let began = tokio::time::Instant::now();
        let result = runtime
            .retire_captured_idle_builder_until(
                "review",
                &captured(Some("old-workload")),
                began + std::time::Duration::from_millis(50),
            )
            .await;
        let elapsed = began.elapsed();
        server.await.unwrap();
        assert!(
            elapsed < std::time::Duration::from_millis(150),
            "{phase} escaped shared deadline: {elapsed:?}, {result:?}"
        );
        let Error::Timeout(message) = result.unwrap_err() else {
            panic!("shared deadline must classify timeout")
        };
        assert_eq!(message.contains("unknown"), phase != "read");
        let actual = requests.lock().unwrap();
        if phase == "read" {
            assert_eq!(actual.len(), 1);
        } else if phase == "delete" {
            assert_eq!(
                actual
                    .iter()
                    .filter(|row| row.starts_with("DELETE "))
                    .count(),
                1
            );
        } else {
            assert_eq!(
                actual
                    .iter()
                    .filter(|row| row.starts_with("DELETE "))
                    .count(),
                3
            );
        }
    }
}
