use container_runtime_api::{AgentContainerRuntime, ContainerRuntimeError};
use shared_types::{ContainerBasicInfo, ServiceType};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) fn captured(workload_uid: Option<&str>) -> ContainerBasicInfo {
    ContainerBasicInfo {
        container_id: "old-id".into(),
        container_name: "rcoder-app-builder-review".into(),
        container_ip: "127.0.0.1".into(),
        internal_port: 60000,
        external_port: 0,
        project_id: "review".into(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        service_url: String::new(),
        workload_uid: workload_uid.map(str::to_owned),
    }
}

pub(super) async fn request(stream: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    let mut buffer = [0; 2048];
    let (offset, length) = loop {
        let count = stream.read(&mut buffer).await.unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let length = String::from_utf8_lossy(&bytes[..offset])
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            break (offset + 4, length);
        }
    };
    while bytes.len() < offset + length {
        let count = stream.read(&mut buffer).await.unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    (
        String::from_utf8_lossy(&bytes[..offset])
            .lines()
            .next()
            .unwrap()
            .to_owned(),
        bytes[offset..offset + length].to_vec(),
    )
}

pub(super) async fn reply(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    body: serde_json::Value,
) {
    let body = body.to_string();
    stream.write_all(format!("HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
}

#[tokio::test]
async fn idle_docker_captured_instance_never_deletes_same_name_replacement() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let server = tokio::spawn(async move {
        loop {
            let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept())
                    .await
            else {
                break;
            };
            let (first, _) = request(&mut stream).await;
            recorded.lock().unwrap().push(first.clone());
            if first.contains("/exec") {
                reply(
                    &mut stream,
                    400,
                    serde_json::json!({"message":"owner unavailable"}),
                )
                .await;
            } else if first.starts_with("DELETE ") {
                reply(&mut stream, 200, serde_json::json!({})).await;
            } else {
                assert!(first.starts_with("GET "), "{first}");
                reply(&mut stream, 200, serde_json::json!({
                    "Id":"replacement-id", "Name":"/rcoder-app-builder-review",
                    "Created":"2026-10-12T00:00:00Z", "State":{"Running":true,"Status":"running"},
                    "Config":{"Labels":{"service-type":ServiceType::UserappBuilder.to_string(),"identifier":"review"}},
                    "NetworkSettings":{"Networks":{"test":{"IPAddress":"127.0.0.1"}}}
                })).await;
            }
        }
    });
    let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
    let actor_task = tokio::spawn(actor.run());
    let runtime =
        super::super::docker_runtime::DockerRuntime::new(Arc::new(crate::DockerManager {
            docker: bollard::Docker::connect_with_http(
                &format!("http://{address}"),
                2,
                bollard::API_DEFAULT_VERSION,
            )
            .unwrap(),
            config: crate::DockerManagerConfig::default(),
            containers,
            main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
            api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
        }));
    let result = runtime
        .stop_idle_builder_compute("review", &captured(None))
        .await;
    actor_task.abort();
    server.await.unwrap();
    let actual = requests.lock().unwrap();
    assert!(
        matches!(result, Err(ContainerRuntimeError::Conflict(_))),
        "replacement must conflict: {result:?}; requests={actual:?}"
    );
    assert!(
        actual.iter().all(|request| request.starts_with("GET ")),
        "no replacement drain/delete: {actual:?}"
    );
}

#[tokio::test]
async fn idle_docker_exact_retirement_preserves_all_volumes_and_replacement_cache() {
    for case in [
        "drain-failed",
        "retained-name",
        "missing",
        "foreign-app",
        "foreign-family",
        "inspect-error",
        "remove-error",
        "confirmation-error",
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let server = tokio::spawn(async move {
            let mut removed = false;
            loop {
                let Ok(Ok((mut stream, _))) =
                    tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept())
                        .await
                else {
                    break;
                };
                let (first, body) = request(&mut stream).await;
                recorded.lock().unwrap().push(first.clone());
                assert!(
                    first.contains("/containers/old-id/") || first.contains("/containers/old-id?"),
                    "captured CID only: {first}"
                );
                assert!(!first.contains("/volumes/") && !first.contains("replacement-id"));
                if first.contains("/exec") {
                    let exec: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(
                        exec["Cmd"],
                        serde_json::json!([
                            "app-cli",
                            "--app-cli-container-stop",
                            "/home/user/review"
                        ])
                    );
                    reply(
                        &mut stream,
                        400,
                        serde_json::json!({"message":"owner unavailable"}),
                    )
                    .await;
                } else if first.starts_with("DELETE ") {
                    assert!(
                        first.contains("v=false"),
                        "anonymous volumes must stay: {first}"
                    );
                    assert!(first.contains("force=true"));
                    assert_eq!(body.len(), 0);
                    removed = true;
                    reply(
                        &mut stream,
                        if case == "remove-error" { 403 } else { 200 },
                        serde_json::json!({"message":"remove response"}),
                    )
                    .await;
                } else {
                    assert!(first.starts_with("GET "));
                    let status =
                        if case == "inspect-error" || (removed && case == "confirmation-error") {
                            403
                        } else if case == "missing" || removed {
                            404
                        } else {
                            200
                        };
                    let labels = serde_json::json!({"service-type":if case=="foreign-family" {"user-app"} else {ServiceType::UserappBuilder.container_family_key()}, "identifier":if case=="foreign-app" {"foreign"} else {"review"}});
                    let body = if status == 200 {
                        serde_json::json!({"Id":"old-id","Name":if case=="retained-name" {"/historical-builder-review"} else {"/rcoder-app-builder-review"},"Config":{"Labels":labels},"State":{"Running":true}})
                    } else {
                        serde_json::json!({"message":"captured not available"})
                    };
                    reply(&mut stream, status, body).await;
                }
            }
        });
        let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
        let actor_task = tokio::spawn(actor.run());
        let mut replacement = crate::DockerContainerInfo::new(
            "replacement-id".into(),
            "rcoder-app-builder-review".into(),
            "review".into(),
            "immutable:replacement".into(),
        );
        replacement.service_type = Some(ServiceType::UserappBuilder);
        containers.insert("review".into(), replacement).await;
        let runtime =
            super::super::docker_runtime::DockerRuntime::new(Arc::new(crate::DockerManager {
                docker: bollard::Docker::connect_with_http(
                    &format!("http://{address}"),
                    2,
                    bollard::API_DEFAULT_VERSION,
                )
                .unwrap(),
                config: crate::DockerManagerConfig::default(),
                containers: containers.clone(),
                main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
                api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
            }));
        let mut target = captured(None);
        if case == "retained-name" {
            target.container_name = "historical-builder-review".into();
        }
        let result = runtime.stop_idle_builder_compute("review", &target).await;
        server.await.unwrap();
        assert_eq!(
            containers.get("review").await.unwrap().container_id,
            "replacement-id",
            "retire cache by original physical ID only"
        );
        actor_task.abort();
        let actual = requests.lock().unwrap();
        match case {
            "drain-failed" | "retained-name" => {
                result.unwrap();
                assert!(
                    actual.iter().any(|request| request.contains("/exec")),
                    "bounded owner drain tried"
                );
                assert_eq!(
                    actual
                        .iter()
                        .filter(|request| request.starts_with("DELETE "))
                        .count(),
                    1
                );
                assert!(
                    actual.last().unwrap().contains("/old-id/json"),
                    "confirm absence after remove"
                );
            }
            "missing" => {
                result.unwrap();
                assert_eq!(actual.len(), 1);
            }
            "foreign-app" | "foreign-family" => {
                assert!(
                    matches!(result, Err(ContainerRuntimeError::Conflict(_))),
                    "{case}: {result:?}"
                );
                assert_eq!(actual.len(), 1);
            }
            "inspect-error" => {
                assert!(matches!(result, Err(ContainerRuntimeError::DockerError(_))));
                assert_eq!(actual.len(), 1);
            }
            "remove-error" | "confirmation-error" => {
                assert!(matches!(result, Err(ContainerRuntimeError::DockerError(_))));
                assert_eq!(
                    actual
                        .iter()
                        .filter(|request| request.starts_with("DELETE "))
                        .count(),
                    1
                );
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(feature = "kubernetes")]
pub(super) fn kubernetes_runtime(
    client: kube::Client,
) -> super::super::kubernetes_runtime::KubernetesRuntime {
    use super::super::kubernetes_runtime::{KubernetesRuntime, KubernetesRuntimeConfig};
    KubernetesRuntime {
        client,
        namespace: "review-test".into(),
        config: KubernetesRuntimeConfig {
            namespace: "review-test".into(),
            cluster_domain: "cluster.local".into(),
            pod_ttl_seconds: None,
            image_pull_secret: None,
            service_account_name: "test".into(),
            nfs_server: "unused".into(),
            nfs_path: "/unused".into(),
            storage_class: "unused".into(),
            access_mode: "ReadWriteOnce".into(),
            docker_manager_config: Default::default(),
            kubernetes_config: Default::default(),
            execution_authority: "k8s:test".into(),
        },
        pod_cache: Default::default(),
        subvolume_path_cache: Default::default(),
        event_publisher: Default::default(),
        event_counters: Arc::new(super::super::k8s_event_publisher::PublisherCounters::default()),
    }
}

#[cfg(feature = "kubernetes")]
#[tokio::test]
async fn idle_kubernetes_captured_instance_never_deletes_same_name_replacement() {
    drop(rustls::crypto::ring::default_provider().install_default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let server = tokio::spawn(async move {
        let mut deleted = false;
        loop {
            let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept())
                    .await
            else {
                break;
            };
            let (first, _) = request(&mut stream).await;
            recorded.lock().unwrap().push(first.clone());
            let path = first.split_whitespace().nth(1).unwrap();
            let name = path.split('?').next().unwrap().rsplit('/').next().unwrap();
            if path.contains("/exec?") {
                reply(&mut stream, 400, serde_json::json!({"kind":"Status","apiVersion":"v1","code":400,"reason":"BadRequest","message":"owner unavailable"})).await;
            } else if first.starts_with("DELETE ") {
                deleted = true;
                reply(
                    &mut stream,
                    200,
                    serde_json::json!({"kind":"Status","apiVersion":"v1","status":"Success"}),
                )
                .await;
            } else if path.contains("/pods/") && deleted {
                reply(&mut stream,404,serde_json::json!({"kind":"Status","apiVersion":"v1","code":404,"reason":"NotFound"})).await;
            } else {
                assert!(first.starts_with("GET "), "{first}");
                let kind = if path.contains("/pods/") {
                    "Pod"
                } else if path.contains("/statefulsets/") {
                    "StatefulSet"
                } else {
                    "Service"
                };
                reply(&mut stream,200,serde_json::json!({"kind":kind,"apiVersion":if kind=="StatefulSet" {"apps/v1"} else {"v1"},
                    "metadata":{"name":name,"uid":if kind=="Pod" {"replacement-pod"} else {"replacement-workload"},"resourceVersion":"8",
                        "labels":{"rcoder.io/service-type":ServiceType::UserappBuilder.to_string(),"rcoder.io/identifier":"review"},
                        "ownerReferences":if kind=="Pod" {serde_json::json!([{"apiVersion":"apps/v1","kind":"StatefulSet","name":"rcoder-app-builder-review","uid":"replacement-workload","controller":true}])} else {serde_json::json!([])}}
                })).await;
            }
        }
    });
    let client = kube::Client::try_from(kube::Config::new(
        format!("http://{address}").parse().unwrap(),
    ))
    .unwrap();
    let result = kubernetes_runtime(client)
        .stop_idle_builder_compute("review", &captured(Some("old-workload")))
        .await;
    server.await.unwrap();
    let actual = requests.lock().unwrap();
    assert!(
        matches!(result, Err(ContainerRuntimeError::Conflict(_))),
        "replacement must conflict: {result:?}; requests={actual:?}"
    );
    assert!(
        actual.iter().all(|request| request.starts_with("GET ")),
        "no replacement drain/delete: {actual:?}"
    );
}

#[cfg(feature = "kubernetes")]
#[tokio::test]
async fn idle_kubernetes_exact_retirement_conditions_all_deletes_and_preserves_pvcs() {
    drop(rustls::crypto::ring::default_provider().install_default());
    for case in [
        "drain-failed",
        "retained-name",
        "foreign-service",
        "pod-replaced",
        "workload-replaced",
        "replaced-during-drain",
        "missing",
        "foreign-app",
        "read-error",
        "delete-conflict",
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let server = tokio::spawn(async move {
            let mut drained = false;
            let mut deleted = false;
            loop {
                let Ok(Ok((mut stream, _))) =
                    tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept())
                        .await
                else {
                    break;
                };
                let (first, body) = request(&mut stream).await;
                recorded.lock().unwrap().push(first.clone());
                assert!(
                    !first.contains("persistentvolumeclaims")
                        && !first.contains("persistentvolumes"),
                    "no storage mutation or discovery needed: {first}"
                );
                let path = first.split_whitespace().nth(1).unwrap();
                let name = path.split('?').next().unwrap().rsplit('/').next().unwrap();
                let kind = if path.contains("/pods/") {
                    "Pod"
                } else if path.contains("/statefulsets/") {
                    "StatefulSet"
                } else {
                    "Service"
                };
                if path.contains("/exec?") {
                    drained = true;
                    assert!(
                        path.contains("old-id"),
                        "drain script must fence captured Pod UID: {path}"
                    );
                    reply(&mut stream,400,serde_json::json!({"kind":"Status","apiVersion":"v1","code":400,"reason":"BadRequest","message":"owner unavailable"})).await;
                } else if first.starts_with("DELETE ") {
                    assert!(drained, "capture and owner drain precede deletion");
                    assert_ne!(kind, "Pod", "no unconditioned direct Pod deletion");
                    let params: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    let expected = if kind == "StatefulSet" {
                        "old-workload".to_owned()
                    } else {
                        format!("old-{name}")
                    };
                    assert_eq!(params["preconditions"]["uid"], expected);
                    assert_eq!(params["preconditions"]["resourceVersion"], "7");
                    if kind == "StatefulSet" {
                        assert_eq!(params["propagationPolicy"], "Foreground");
                    }
                    deleted = true;
                    let code = if case == "delete-conflict" { 409 } else { 200 };
                    reply(&mut stream,code,serde_json::json!({"kind":"Status","apiVersion":"v1","code":code,"reason":if code==409 {"Conflict"} else {"Success"},"message":"captured removal"})).await;
                } else {
                    assert!(first.starts_with("GET "));
                    let status = if case == "read-error" {
                        403
                    } else if kind == "Pod" && (case == "missing" || deleted) {
                        404
                    } else {
                        200
                    };
                    if status != 200 {
                        reply(&mut stream,status,serde_json::json!({"kind":"Status","apiVersion":"v1","code":status,"reason":if status==404 {"NotFound"} else {"Forbidden"},"message":"captured observation"})).await;
                        continue;
                    }
                    let uid = if kind == "Pod" {
                        if case == "pod-replaced" || (drained && case == "replaced-during-drain") {
                            "new-pod".into()
                        } else {
                            "old-id".into()
                        }
                    } else if kind == "StatefulSet" {
                        if case == "workload-replaced" {
                            "new-workload".into()
                        } else {
                            "old-workload".into()
                        }
                    } else {
                        format!("old-{name}")
                    };
                    reply(&mut stream,200,serde_json::json!({"apiVersion":if kind=="StatefulSet" {"apps/v1"} else {"v1"},"kind":kind,
                        "metadata":{"name":name,"uid":uid,"resourceVersion":"7",
                            "labels":{"rcoder.io/service-type":ServiceType::UserappBuilder.container_family_key(),"rcoder.io/identifier":if case=="foreign-app" {"another"} else {"review"}},
                            "ownerReferences":if kind=="Pod" {serde_json::json!([{"apiVersion":"apps/v1","kind":"StatefulSet","name":if case=="retained-name" {"historical-builder-review"} else {"rcoder-app-builder-review"},"uid":"old-workload","controller":true}])} else {serde_json::json!([])}},
                        "spec":if kind=="Service" {serde_json::json!({"clusterIP":if name.ends_with("-headless") {"None"} else {"10.0.0.1"},"selector":{"app.kubernetes.io/name":ServiceType::UserappBuilder.container_family_key(),"app.kubernetes.io/instance":"review","app.kubernetes.io/managed-by":"rcoder-runtime","rcoder.io/identifier":if case=="foreign-service" {"other"} else {"review"}}})} else {serde_json::json!({})}
                    })).await;
                }
            }
        });
        let client = kube::Client::try_from(kube::Config::new(
            format!("http://{address}").parse().unwrap(),
        ))
        .unwrap();
        let mut target = captured(Some("old-workload"));
        if case == "retained-name" {
            target.container_name = "historical-builder-review".into();
        }
        let result = kubernetes_runtime(client)
            .stop_idle_builder_compute("review", &target)
            .await;
        server.await.unwrap();
        let actual = requests.lock().unwrap();
        match case {
            "drain-failed" | "retained-name" => {
                result.unwrap();
                assert_eq!(
                    actual
                        .iter()
                        .filter(|row| row.starts_with("DELETE "))
                        .count(),
                    3
                );
            }
            "missing" => {
                result.unwrap();
                assert!(actual.iter().all(|row| row.starts_with("GET ")));
            }
            "pod-replaced" | "workload-replaced" | "foreign-app" | "foreign-service" => {
                assert!(
                    matches!(result, Err(ContainerRuntimeError::Conflict(_))),
                    "{case}: {result:?}"
                );
                assert!(actual.iter().all(|row| row.starts_with("GET ")));
            }
            "replaced-during-drain" => {
                assert!(matches!(result, Err(ContainerRuntimeError::Conflict(_))));
                assert!(actual.iter().all(|row| !row.starts_with("DELETE ")));
            }
            "delete-conflict" => {
                assert!(
                    matches!(result, Err(ContainerRuntimeError::RequestRejected(ref rejection)) if rejection.status == 409),
                    "exact CAS conflict stays a typed rejected write: {result:?}"
                );
                assert_eq!(
                    actual
                        .iter()
                        .filter(|row| row.starts_with("DELETE "))
                        .count(),
                    1
                );
            }
            "read-error" => {
                assert!(result.is_err());
                assert_eq!(actual.len(), 1);
            }
            _ => unreachable!(),
        }
    }
}
