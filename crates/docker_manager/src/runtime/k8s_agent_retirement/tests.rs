use super::*;
use container_runtime_api::AgentContainerRuntime;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
mod app_id_hint_tests;

fn runtime(client: kube::Client) -> KubernetesRuntime {
    KubernetesRuntime {
        client,
        namespace: "retirement-test".into(),
        config: super::super::kubernetes_runtime::KubernetesRuntimeConfig {
            namespace: "retirement-test".into(),
            cluster_domain: "cluster.local".into(),
            pod_ttl_seconds: None,
            image_pull_secret: None,
            service_account_name: "test".into(),
            nfs_server: "unused".into(),
            nfs_path: "/unused".into(),
            storage_class: "unused".into(),
            access_mode: "ReadWriteMany".into(),
            docker_manager_config: Default::default(),
            kubernetes_config: Default::default(),
            execution_authority: "test".into(),
        },
        pod_cache: Default::default(),
        subvolume_path_cache: Default::default(),
        event_publisher: Default::default(),
        event_counters: Arc::new(super::super::k8s_event_publisher::PublisherCounters::default()),
    }
}

#[derive(Clone, Copy)]
enum Case {
    ControllerOnly,
    RunningController,
    BarePod,
    ServiceOnly,
    ServiceFailure,
    ObserveFailure,
    Changed,
    Foreign,
    ForeignController,
    PartialList(Kind, bool),
    PartialFinalPods(bool),
    UnsafeRetention,
    OwnedPvc,
    PodOwnedPvc,
    IndependentPvc,
    PvcObservationFailure(bool),
    InitialWatchPending,
    DeleteAckLost,
    TerminatingWaitFailure(Kind),
}

struct Fixture {
    runtime: KubernetesRuntime,
    recorded: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    objects: Arc<Mutex<Vec<serde_json::Value>>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn fixture(case: Case) -> Fixture {
    drop(rustls::crypto::ring::default_provider().install_default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = kube::Config::new(
        format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap(),
    );
    config.default_retry = false;
    let runtime = runtime(kube::Client::try_from(config).unwrap());
    let name = runtime
        .agent_workload_name("owner", &ServiceType::ComputerAgentRunner)
        .unwrap();
    let labels = super::super::k8s_service::build_standard_labels(
        "owner",
        &ServiceType::ComputerAgentRunner,
    );
    let object = |kind: &str, name: String, uid: &str| {
        serde_json::json!({
        "apiVersion":if kind=="StatefulSet" {"apps/v1"} else {"v1"}, "kind":kind,
        "metadata":{"namespace":"retirement-test","name":name,"uid":uid,"resourceVersion":"7","labels":labels,
            "creationTimestamp":"2026-10-01T00:00:00Z"}})
    };
    let mut objects = Vec::new();
    if matches!(
        case,
        Case::ControllerOnly
            | Case::RunningController
            | Case::ServiceFailure
            | Case::ObserveFailure
            | Case::Changed
            | Case::Foreign
            | Case::ForeignController
            | Case::UnsafeRetention
            | Case::OwnedPvc
            | Case::IndependentPvc
            | Case::DeleteAckLost
            | Case::TerminatingWaitFailure(Kind::StatefulSet)
    ) {
        let mut controller = object("StatefulSet", name.clone(), "old-sts");
        controller["spec"] = serde_json::json!({"replicas":if matches!(case, Case::RunningController) {1} else {0},"serviceName":"test","selector":{"matchLabels":labels},"template":{"metadata":{"labels":labels},"spec":{"containers":[{"name":"agent","image":"test"}]}}});
        if matches!(case, Case::UnsafeRetention) {
            controller["spec"]["persistentVolumeClaimRetentionPolicy"] =
                serde_json::json!({"whenDeleted":"Delete","whenScaled":"Retain"});
            controller["spec"]["volumeClaimTemplates"] = serde_json::json!([{"metadata":{"name":"data"},"spec":{"accessModes":["ReadWriteOnce"],"resources":{"requests":{"storage":"1Gi"}}}}]);
        }
        objects.push(controller);
    }
    if matches!(
        case,
        Case::BarePod
            | Case::ForeignController
            | Case::PodOwnedPvc
            | Case::TerminatingWaitFailure(Kind::Pod)
    ) {
        let mut pod = object("Pod", format!("{name}-0"), "bare-pod");
        pod["spec"] = serde_json::json!({"containers":[{"name":"agent","image":"test"}]});
        if matches!(case, Case::ForeignController) {
            pod["metadata"]["ownerReferences"] = serde_json::json!([{"apiVersion":"apps/v1","kind":"StatefulSet","name":"foreign-controller","uid":"foreign-sts","controller":true}]);
        }
        objects.push(pod);
    }
    let mut service = object("Service", format!("{name}-svc"), "old-service");
    service["spec"] = serde_json::json!({"selector":labels,"ports":[{"port":50051}]});
    objects.push(service);
    if matches!(
        case,
        Case::OwnedPvc | Case::PodOwnedPvc | Case::IndependentPvc
    ) {
        let mut claim = object(
            "PersistentVolumeClaim",
            "retained-data".into(),
            "retained-pvc",
        );
        claim["spec"] = serde_json::json!({"accessModes":["ReadWriteOnce"],"resources":{"requests":{"storage":"1Gi"}}});
        if matches!(case, Case::OwnedPvc | Case::PodOwnedPvc) {
            let (api, kind, owner_name, uid) = if matches!(case, Case::OwnedPvc) {
                ("apps/v1", "StatefulSet", name.clone(), "old-sts")
            } else {
                ("v1", "Pod", format!("{name}-0"), "bare-pod")
            };
            claim["metadata"]["ownerReferences"] = serde_json::json!([{"apiVersion":api,"kind":kind,"name":owner_name,"uid":uid,"controller":true}]);
        }
        objects.push(claim);
    }
    let objects = Arc::new(Mutex::new(objects));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let saved = recorded.clone();
    let live = objects.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let live = live.clone();
            let saved = saved.clone();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut buffer = [0; 2048];
                let (first, body) = loop {
                    let count = stream.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break (
                                headers.lines().next().unwrap().to_owned(),
                                bytes[end + 4..end + 4 + length].to_vec(),
                            );
                        }
                    }
                };
                let body = if body.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&body).unwrap()
                };
                saved.lock().unwrap().push((first.clone(), body.clone()));
                assert!(
                    !first.contains("persistentvolumeclaims")
                        || (!first.starts_with("DELETE ") && !first.starts_with("PATCH ")),
                    "compute retirement must never mutate a PVC"
                );
                let url = reqwest::Url::parse(&format!(
                    "http://test{}",
                    first.split_whitespace().nth(1).unwrap()
                ))
                .unwrap();
                let path = url.path();
                let (plural, kind) = if path.contains("/statefulsets") {
                    ("statefulsets", "StatefulSet")
                } else if path.contains("/pods") {
                    ("pods", "Pod")
                } else if path.contains("/persistentvolumeclaims") {
                    ("persistentvolumeclaims", "PersistentVolumeClaim")
                } else {
                    ("services", "Service")
                };
                let name = path.split(&format!("/{plural}/")).nth(1);
                if matches!(case, Case::InitialWatchPending)
                    && url
                        .query_pairs()
                        .any(|(key, value)| key == "watch" && value == "true")
                {
                    // Keep the actual initial watch body pending: no InitDone
                    // can make the Controller's reflector store ready.
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
                        .await
                        .unwrap();
                    let mut byte = [0; 1];
                    match stream.read(&mut byte).await {
                        Ok(0) => {}
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::ConnectionAborted
                            ) => {}
                        result => panic!("pending watch connection did not close: {result:?}"),
                    }
                    saved
                        .lock()
                        .unwrap()
                        .push((format!("WATCH CLOSED {kind}"), serde_json::Value::Null));
                    return;
                }
                let mut code = 200;
                let body = {
                    let mut live = live.lock().unwrap();
                    let response = if first.starts_with("DELETE ") {
                        assert_eq!(body["preconditions"]["resourceVersion"], "7");
                        if kind == "StatefulSet" && matches!(case, Case::DeleteAckLost) {
                            let object = live
                                .iter_mut()
                                .find(|object| {
                                    object["kind"] == kind
                                        && object["metadata"]["name"] == name.unwrap()
                                })
                                .unwrap();
                            object["metadata"]["deletionTimestamp"] = "2026-10-22T00:00:00Z".into();
                            object["metadata"]["resourceVersion"] = "8".into();
                            // The API committed DELETE, but the response was lost.
                            // Returning drops this HTTP connection without an ACK.
                            return;
                        }
                        if kind == "Service" && matches!(case, Case::ServiceFailure) {
                            code = 403;
                            serde_json::json!({"apiVersion":"v1","kind":"Status","code":403,"reason":"Forbidden","message":"service forbidden"})
                        } else {
                            let target = name.unwrap();
                            live.retain(|object| {
                                object["kind"] != kind || object["metadata"]["name"] != target
                            });
                            serde_json::json!({"kind":"Status","apiVersion":"v1","status":"Success"})
                        }
                    } else if first.starts_with("PATCH ") {
                        assert!(body.get("spec").is_none(), "fence must not roll a template");
                        let object = live
                            .iter_mut()
                            .find(|object| {
                                object["kind"] == kind
                                    && object["metadata"]["name"] == name.unwrap()
                            })
                            .unwrap();
                        let revision = object["metadata"]["resourceVersion"]
                            .as_str()
                            .unwrap()
                            .parse::<u32>()
                            .unwrap()
                            + 1;
                        object["metadata"]["resourceVersion"] = revision.to_string().into();
                        object["metadata"]["annotations"] = body["metadata"]["annotations"].clone();
                        object.clone()
                    } else if let Some(name) = name {
                        if let Some(object) = live.iter().find(|object| {
                            object["kind"] == kind && object["metadata"]["name"] == name
                        }) {
                            object.clone()
                        } else {
                            code = 404;
                            serde_json::json!({"apiVersion":"v1","kind":"Status","code":404,"reason":"NotFound","message":"gone"})
                        }
                    } else {
                        let field = url
                            .query_pairs()
                            .find(|(key, _)| key == "fieldSelector")
                            .map(|(_, value)| value.into_owned());
                        let labels = url
                            .query_pairs()
                            .find(|(key, _)| key == "labelSelector")
                            .map(|(_, value)| value.into_owned());
                        let pending_kind = match case {
                            Case::TerminatingWaitFailure(Kind::StatefulSet) => Some("StatefulSet"),
                            Case::TerminatingWaitFailure(Kind::Pod) => Some("Pod"),
                            Case::TerminatingWaitFailure(Kind::Service) => Some("Service"),
                            _ => None,
                        };
                        if matches!(case, Case::DeleteAckLost)
                            && kind == "StatefulSet"
                            && field.is_some()
                            && url
                                .query_pairs()
                                .any(|(key, value)| key == "watch" && value == "true")
                        {
                            let position = live
                                .iter()
                                .position(|object| object["kind"] == kind)
                                .expect("the accepted deletion is still pending before GC");
                            let deleted = live.remove(position);
                            serde_json::json!({"type":"DELETED", "object":deleted})
                        } else if (field.is_some()
                            && (matches!(case, Case::ObserveFailure) || pending_kind == Some(kind)))
                            || (kind == "PersistentVolumeClaim"
                                && matches!(case, Case::PvcObservationFailure(false)))
                        {
                            code = 403;
                            serde_json::json!({"apiVersion":"v1","kind":"Status","code":403,"reason":"Forbidden","message":"observation forbidden"})
                        } else {
                            let items: Vec<_> = live
                                .iter()
                                .filter(|object| {
                                    object["kind"] == kind
                                        && labels.as_deref().is_none_or(|selector| {
                                            selector.split(',').all(|entry| {
                                                entry.split_once('=').is_some_and(|(key, value)| {
                                                    object["metadata"]["labels"][key] == value
                                                })
                                            })
                                        })
                                        && field.as_deref().is_none_or(|field| {
                                            object["metadata"]["name"]
                                                == field.trim_start_matches("metadata.name=")
                                        })
                                })
                                .cloned()
                                .collect();
                            let partial = match case {
                                Case::PartialList(target, remaining) => {
                                    let target = match target {
                                        Kind::Container => "Container",
                                        Kind::StatefulSet => "StatefulSet",
                                        Kind::Pod => "Pod",
                                        Kind::Service => "Service",
                                    };
                                    (target == kind).then_some(remaining)
                                }
                                Case::PartialFinalPods(remaining)
                                    if kind == "Pod" && labels.is_none() =>
                                {
                                    Some(remaining)
                                }
                                Case::PvcObservationFailure(true)
                                    if kind == "PersistentVolumeClaim" =>
                                {
                                    Some(false)
                                }
                                _ => None,
                            };
                            let mut metadata = serde_json::json!({"resourceVersion":"7"});
                            if let Some(remaining) = partial {
                                if remaining {
                                    metadata["remainingItemCount"] = 1.into();
                                } else {
                                    metadata["continue"] = "unconsumed-page".into();
                                }
                            }
                            serde_json::json!({"apiVersion":if kind=="StatefulSet"{"apps/v1"}else{"v1"},"kind":format!("{kind}List"),"metadata":metadata,"items":items})
                        }
                    };
                    response.to_string()
                };
                stream.write_all(format!("HTTP/1.1 {code} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            });
        }
    });
    Fixture {
        runtime,
        recorded,
        objects,
        server,
    }
}

#[tokio::test]
async fn partial_controller_pod_or_service_inventory_never_authorizes_a_write() {
    for kind in [Kind::StatefulSet, Kind::Pod, Kind::Service] {
        for remaining in [false, true] {
            let fixture = fixture(Case::PartialList(kind, remaining)).await;
            let inventory = fixture
                .runtime
                .agent_resource_inventory()
                .await
                .unwrap_err();
            assert!(inventory.to_string().contains("incomplete"));
            let capture = fixture
                .runtime
                .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
                .await
                .unwrap_err();
            assert!(capture.to_string().contains("incomplete"));
            assert!(
                !fixture
                    .recorded
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(request, _)| request.starts_with("DELETE ")
                        || request.starts_with("PATCH "))
            );
        }
    }
}

#[tokio::test]
async fn partial_final_pod_list_cannot_prove_service_selector_or_compute_absence() {
    for remaining in [false, true] {
        let fixture = fixture(Case::PartialFinalPods(remaining)).await;
        let snapshot = fixture
            .runtime
            .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
            .await
            .unwrap();
        assert!(snapshot.is_remnant());
        let error = fixture
            .runtime
            .retire_agent_remnants(&snapshot)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("incomplete Pod"));
        assert!(!fixture.recorded.lock().unwrap().iter().any(|(request, _)| request.starts_with("DELETE ") || request.starts_with("PATCH ")));
    }
}

#[tokio::test]
async fn unsafe_statefulset_claim_retention_prevents_any_retirement_write() {
    let fixture = fixture(Case::UnsafeRetention).await;
    let snapshot = fixture
        .runtime
        .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
        .await
        .unwrap();
    let error = fixture
        .runtime
        .retire_agent_remnants(&snapshot)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("would delete volume claims"));
    assert!(
        !fixture
            .recorded
            .lock()
            .unwrap()
            .iter()
            .any(|(request, _)| request.starts_with("DELETE ") || request.starts_with("PATCH "))
    );
}

#[tokio::test]
async fn pvc_gc_dependencies_or_unknown_pvc_inventory_preserve_compute() {
    for case in [
        Case::OwnedPvc,
        Case::PodOwnedPvc,
        Case::PvcObservationFailure(false),
        Case::PvcObservationFailure(true),
    ] {
        let fixture = fixture(case).await;
        let snapshot = fixture
            .runtime
            .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
            .await
            .unwrap();
        assert!(
            fixture
                .runtime
                .stop_captured_agent(&snapshot)
                .await
                .is_err()
        );
        assert!(!fixture.recorded.lock().unwrap().iter().any(|(request, _)| request.starts_with("DELETE ") || request.starts_with("PATCH ")));
    }
}

#[tokio::test]
async fn independent_pvc_identity_is_preserved_after_compute_retirement() {
    let fixture = fixture(Case::IndependentPvc).await;
    let snapshot = fixture
        .runtime
        .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .runtime
            .retire_agent_remnants(&snapshot)
            .await
            .unwrap(),
        AgentStopOutcome::Complete
    );
    let remaining = fixture.objects.lock().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0]["kind"], "PersistentVolumeClaim");
    assert_eq!(remaining[0]["metadata"]["uid"], "retained-pvc");
    assert_eq!(remaining[0]["metadata"]["resourceVersion"], "7");
}

#[tokio::test]
async fn legacy_agent_stop_cannot_hide_physical_or_service_failures() {
    for case in [Case::ObserveFailure, Case::ServiceFailure] {
        let fixture = fixture(case).await;
        let error = fixture
            .runtime
            .stop_container_by_identifier_inner("owner", &ServiceType::ComputerAgentRunner)
            .await
            .expect_err("legacy Agent stop must use the same confirmed retirement boundary");
        assert!(
            error.to_string().to_ascii_lowercase().contains("forbidden"),
            "original error was lost: {error}"
        );
        let requests = fixture.recorded.lock().unwrap();
        assert!(
            requests
                .iter()
                .any(|(request, _)| request.starts_with("DELETE ")
                    && request.contains("/statefulsets/")),
            "the fixture must reach actual compute retirement"
        );
        if matches!(case, Case::ObserveFailure) {
            assert!(
                !requests
                    .iter()
                    .any(|(request, _)| request.starts_with("DELETE ")
                        && request.contains("/services/")),
                "unconfirmed compute must not advance to Service retirement"
            );
        } else {
            assert!(
                error
                    .to_string()
                    .contains("Service cleanup remains pending")
            );
        }
    }
}

#[tokio::test]
async fn legacy_agent_stop_rejects_production_userapp_without_runtime_writes() {
    let fixture = fixture(Case::ServiceOnly).await;
    assert!(
        fixture
            .runtime
            .stop_container_by_identifier_inner("owner", &ServiceType::Userapp)
            .await
            .is_err()
    );
    assert!(fixture.recorded.lock().unwrap().is_empty());
}

#[tokio::test]
async fn inventory_and_retirement_cover_controller_only_bare_pod_and_service_only() {
    for case in [Case::ControllerOnly, Case::BarePod, Case::ServiceOnly] {
        let fixture = fixture(case).await;
        let snapshot = fixture
            .runtime
            .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
            .await
            .unwrap();
        assert_eq!(snapshot.services.len(), 1);
        if matches!(case, Case::ControllerOnly) {
            assert!(snapshot.controller.is_some());
            assert!(snapshot.pods.is_empty());
        }
        if matches!(case, Case::BarePod) {
            assert!(!snapshot.is_remnant());
        }
        let result = if snapshot.is_remnant() {
            fixture.runtime.retire_agent_remnants(&snapshot).await
        } else {
            fixture.runtime.stop_captured_agent(&snapshot).await
        }
        .unwrap();
        assert_eq!(result, AgentStopOutcome::Complete);
        assert!(fixture.objects.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn service_failure_preserves_compute_gone_and_original_failure() {
    let fixture = fixture(Case::ServiceFailure).await;
    let snapshot = fixture
        .runtime
        .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
        .await
        .unwrap();
    let outcome = fixture
        .runtime
        .stop_captured_agent(&snapshot)
        .await
        .unwrap();
    let AgentStopOutcome::ComputeGoneServicesPending { pending } = outcome else {
        panic!("service error was swallowed")
    };
    assert_eq!(pending.len(), 1);
    assert!(pending[0].error.contains("forbidden"));
    assert_eq!(fixture.objects.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unconfirmed_exit_is_error_and_does_not_cleanup_services() {
    let fixture = fixture(Case::ObserveFailure).await;
    let snapshot = fixture
        .runtime
        .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
        .await
        .unwrap();
    assert!(
        fixture
            .runtime
            .stop_captured_agent(&snapshot)
            .await
            .is_err()
    );
    assert!(
        !fixture
            .recorded
            .lock()
            .unwrap()
            .iter()
            .any(|(request, _)| request.starts_with("DELETE ") && request.contains("/services/"))
    );
}

#[tokio::test]
async fn active_controller_and_replaced_versions_cannot_be_automatically_retired() {
    let active_fixture = fixture(Case::RunningController).await;
    let snapshot = active_fixture
        .runtime
        .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
        .await
        .unwrap();
    assert!(
        active_fixture
            .runtime
            .retire_agent_remnants(&snapshot)
            .await
            .is_err()
    );
    assert!(
        !active_fixture
            .recorded
            .lock()
            .unwrap()
            .iter()
            .any(|(request, _)| request.starts_with("DELETE "))
    );
    for case in [Case::Changed, Case::Foreign] {
        let fixture = fixture(case).await;
        let snapshot = fixture
            .runtime
            .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
            .await
            .unwrap();
        if matches!(case, Case::Changed) {
            fixture.objects.lock().unwrap()[0]["metadata"]["resourceVersion"] = "8".into();
        } else {
            fixture.objects.lock().unwrap()[0]["metadata"]["labels"]["rcoder.io/identifier"] =
                "another".into();
        }
        assert!(
            fixture
                .runtime
                .stop_captured_agent(&snapshot)
                .await
                .is_err()
        );
        assert!(
            !fixture
                .recorded
                .lock()
                .unwrap()
                .iter()
                .any(|(request, _)| request.starts_with("DELETE "))
        );
    }
}

#[tokio::test]
async fn admission_fence_changes_only_metadata_and_invalidates_old_deletion_version() {
    let fixture = fixture(Case::ControllerOnly).await;
    let old = fixture
        .runtime
        .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
        .await
        .unwrap();
    let current = fixture
        .runtime
        .refresh_agent_retirement_fence(&old)
        .await
        .unwrap();
    assert_eq!(
        current
            .controller
            .as_ref()
            .unwrap()
            .resource_version
            .as_deref(),
        Some("8")
    );
    assert!(fixture.runtime.stop_captured_agent(&old).await.is_err());
    assert!(
        !fixture
            .recorded
            .lock()
            .unwrap()
            .iter()
            .any(|(request, _)| request.starts_with("DELETE "))
    );
}

#[tokio::test]
async fn foreign_controller_cannot_be_deleted_from_same_scope_labels() {
    let fixture = fixture(Case::ForeignController).await;
    assert!(
        fixture
            .runtime
            .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
            .await
            .is_err()
    );
    assert!(
        !fixture
            .recorded
            .lock()
            .unwrap()
            .iter()
            .any(|(request, _)| request.starts_with("DELETE "))
    );
}

#[tokio::test]
async fn admission_fence_can_advance_current_rv_but_never_a_replacement_uid() {
    let fixture = fixture(Case::ControllerOnly).await;
    let old = fixture
        .runtime
        .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
        .await
        .unwrap();
    fixture.objects.lock().unwrap()[0]["metadata"]["resourceVersion"] = "8".into();
    let updated = fixture
        .runtime
        .refresh_agent_retirement_fence(&old)
        .await
        .unwrap();
    assert_eq!(
        updated.controller.unwrap().resource_version.as_deref(),
        Some("9")
    );
    fixture.objects.lock().unwrap()[0]["metadata"]["uid"] = "successor".into();
    assert!(
        fixture
            .runtime
            .refresh_agent_retirement_fence(&old)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn dropping_event_receiver_stops_and_joins_all_resource_controllers() {
    let fixture = fixture(Case::ServiceOnly).await;
    let events = fixture
        .runtime
        .agent_resource_events(
            tokio_util::sync::CancellationToken::new(),
            std::time::Duration::from_secs(30),
        )
        .await
        .unwrap();
    drop(events.receiver);
    tokio::time::timeout(std::time::Duration::from_secs(3), events.task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn pending_initial_watches_are_closed_and_joined_on_shutdown_or_receiver_drop() {
    for drop_receiver in [false, true] {
        let fixture = fixture(Case::InitialWatchPending).await;
        let shutdown = tokio_util::sync::CancellationToken::new();
        let events = fixture
            .runtime
            .agent_resource_events(shutdown.clone(), std::time::Duration::from_secs(30))
            .await
            .unwrap();
        // Preserve the existing three-second stop budget; all three HTTP watch
        // requests must be live before the cancellation path is exercised.
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let all_watching = {
                    let requests = fixture.recorded.lock().unwrap();
                    ["statefulsets", "pods", "services"].iter().all(|kind| {
                        requests.iter().any(|(request, _)| {
                            request.starts_with("GET ")
                                && request.contains(&format!("/{kind}?"))
                                && request.contains("watch=true")
                        })
                    })
                };
                if all_watching {
                    break;
                }
                tokio::task::yield_now().await;
            }
            let mut receiver = Some(events.receiver);
            if drop_receiver {
                drop(receiver.take());
            } else {
                shutdown.cancel();
            }
            events.task.await.unwrap();
            loop {
                let closed = fixture
                    .recorded
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(request, _)| request.starts_with("WATCH CLOSED "))
                    .count();
                if closed == 3 {
                    break;
                }
                tokio::task::yield_now().await;
            }
            drop(receiver);
        })
        .await
        .expect("all three initial watch connections must close and their run futures join");
    }
}

#[tokio::test]
async fn service_only_selected_foreign_unmanaged_pod_is_not_automatic_cleanup_permission() {
    let fixture = fixture(Case::ServiceOnly).await;
    {
        let mut objects = fixture.objects.lock().unwrap();
        objects[0]["spec"]["selector"] = serde_json::json!({"app":"unmanaged-execution"});
        objects.push(serde_json::json!({"apiVersion":"v1","kind":"Pod",
            "metadata":{"namespace":"retirement-test","name":"foreign-worker-0","uid":"foreign-pod","resourceVersion":"7",
                "labels":{"app":"unmanaged-execution"},"creationTimestamp":"2026-10-01T00:00:00Z"},
            "spec":{"containers":[{"name":"foreign","image":"test"}]}}));
    }
    let snapshot = fixture
        .runtime
        .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
        .await
        .unwrap();
    assert!(
        snapshot.pods.is_empty(),
        "managed inventory excludes the foreign execution"
    );
    assert!(snapshot.is_remnant());
    assert!(
        fixture
            .runtime
            .retire_agent_remnants(&snapshot)
            .await
            .is_err()
    );
    assert!(
        !fixture
            .recorded
            .lock()
            .unwrap()
            .iter()
            .any(|(request, _)| request.starts_with("DELETE "))
    );
    assert_eq!(fixture.objects.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn service_only_unknown_manual_and_external_routes_are_preserved_automatically() {
    for routing in [
        serde_json::json!({}),
        serde_json::json!({"selector":{}}),
        serde_json::json!({"type":"ExternalName","externalName":"outside.example"}),
    ] {
        let fixture = fixture(Case::ServiceOnly).await;
        fixture.objects.lock().unwrap()[0]["spec"] = routing;
        let snapshot = fixture
            .runtime
            .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
            .await
            .unwrap();
        assert!(
            fixture
                .runtime
                .retire_agent_remnants(&snapshot)
                .await
                .is_err()
        );
        assert!(
            !fixture
                .recorded
                .lock()
                .unwrap()
                .iter()
                .any(|(request, _)| request.starts_with("DELETE "))
        );
        assert_eq!(fixture.objects.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn unmanaged_canonical_controller_is_not_service_only_absence() {
    let fixture = fixture(Case::ServiceOnly).await;
    let name = fixture
        .runtime
        .agent_workload_name("owner", &ServiceType::ComputerAgentRunner)
        .unwrap();
    fixture.objects.lock().unwrap().push(serde_json::json!({"apiVersion":"apps/v1","kind":"StatefulSet",
        "metadata":{"namespace":"retirement-test","name":name,"uid":"foreign-sts","resourceVersion":"7",
            "creationTimestamp":"2026-10-01T00:00:00Z"},
        "spec":{"replicas":1,"serviceName":"foreign","selector":{"matchLabels":{"app":"foreign"}},
            "template":{"metadata":{"labels":{"app":"foreign"}},"spec":{"containers":[{"name":"foreign","image":"test"}]}}}}));
    assert!(
        fixture
            .runtime
            .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
            .await
            .is_err()
    );
    assert!(
        !fixture
            .recorded
            .lock()
            .unwrap()
            .iter()
            .any(|(request, _)| request.starts_with("DELETE "))
    );
}

#[tokio::test]
async fn pod_scope_labels_do_not_override_a_foreign_controller_name() {
    let fixture = fixture(Case::BarePod).await;
    {
        let mut objects = fixture.objects.lock().unwrap();
        let pod = objects
            .iter_mut()
            .find(|object| object["kind"] == "Pod")
            .unwrap();
        pod["metadata"]["name"] = "foreign-controller-0".into();
        pod["metadata"]["ownerReferences"] = serde_json::json!([{"apiVersion":"apps/v1","kind":"StatefulSet",
            "name":"foreign-controller","uid":"foreign-sts","controller":true}]);
    }
    assert!(
        fixture
            .runtime
            .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
            .await
            .is_err()
    );
    assert!(
        !fixture
            .recorded
            .lock()
            .unwrap()
            .iter()
            .any(|(request, _)| request.starts_with("DELETE "))
    );
}

#[tokio::test]
async fn explicit_invalid_family_label_is_not_a_compatible_missing_legacy_field() {
    let fixture = fixture(Case::ServiceOnly).await;
    fixture.objects.lock().unwrap()[0]["metadata"]["labels"]["rcoder.io/service-type"] =
        "invalid-current-family".into();
    assert!(
        fixture
            .runtime
            .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
            .await
            .is_err()
    );
    assert!(
        !fixture
            .recorded
            .lock()
            .unwrap()
            .iter()
            .any(|(request, _)| request.starts_with("DELETE "))
    );
}

fn mark_terminating(fixture: &Fixture, kind: &str) {
    let mut objects = fixture.objects.lock().unwrap();
    let object = objects
        .iter_mut()
        .find(|object| object["kind"] == kind)
        .unwrap();
    object["metadata"]["deletionTimestamp"] = "2026-10-22T00:00:00Z".into();
    object["metadata"]["resourceVersion"] = "8".into();
}

#[tokio::test]
async fn lost_delete_ack_resumes_original_snapshot_without_repeating_delete() {
    for automatic in [false, true] {
        let fixture = fixture(Case::DeleteAckLost).await;
        fixture
            .objects
            .lock()
            .unwrap()
            .retain(|object| object["kind"] == "StatefulSet");
        let snapshot = fixture
            .runtime
            .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
            .await
            .unwrap();
        let original = snapshot.clone();
        assert!(
            fixture
                .runtime
                .stop_captured_agent(&snapshot)
                .await
                .is_err()
        );
        {
            let objects = fixture.objects.lock().unwrap();
            assert_eq!(objects.len(), 1);
            assert_eq!(objects[0]["metadata"]["resourceVersion"], "8");
            assert!(objects[0]["metadata"]["deletionTimestamp"].is_string());
        }
        let result = if automatic {
            fixture.runtime.retire_agent_remnants(&snapshot).await
        } else {
            fixture.runtime.stop_captured_agent(&snapshot).await
        }
        .unwrap();
        assert_eq!(result, AgentStopOutcome::Complete);
        assert_eq!(
            snapshot, original,
            "the deletion witness must never be refreshed"
        );
        let requests = fixture.recorded.lock().unwrap();
        let deletes: Vec<_> = requests
            .iter()
            .filter(|(request, _)| request.starts_with("DELETE "))
            .collect();
        assert_eq!(
            deletes.len(),
            1,
            "the unknown first DELETE must not be replayed"
        );
        assert_eq!(deletes[0].1["preconditions"]["uid"], "old-sts");
        assert_eq!(deletes[0].1["preconditions"]["resourceVersion"], "7");
        assert!(
            requests
                .iter()
                .any(|(request, _)| request.contains("watch=true"))
        );
        assert!(
            !requests
                .iter()
                .any(|(request, _)| request.starts_with("PATCH "))
        );
        assert!(fixture.objects.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn terminating_compute_observation_failure_does_not_delete_services() {
    for (kind, name) in [(Kind::StatefulSet, "StatefulSet"), (Kind::Pod, "Pod")] {
        let fixture = fixture(Case::TerminatingWaitFailure(kind)).await;
        let snapshot = fixture
            .runtime
            .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
            .await
            .unwrap();
        mark_terminating(&fixture, name);
        let error = fixture
            .runtime
            .stop_captured_agent(&snapshot)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("observation forbidden"),
            "{error}"
        );
        assert_eq!(fixture.objects.lock().unwrap().len(), 2);
        assert!(!fixture.recorded.lock().unwrap().iter().any(|(request, _)|
            request.starts_with("DELETE ") || request.starts_with("PATCH ")));
    }
}

#[tokio::test]
async fn terminating_service_observation_failure_is_pending_without_a_delete() {
    let fixture = fixture(Case::TerminatingWaitFailure(Kind::Service)).await;
    let snapshot = fixture
        .runtime
        .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
        .await
        .unwrap();
    mark_terminating(&fixture, "Service");
    let result = fixture
        .runtime
        .retire_agent_remnants(&snapshot)
        .await
        .unwrap();
    let AgentStopOutcome::ComputeGoneServicesPending { pending } = result else {
        panic!("unconfirmed Service exit must remain partial");
    };
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].uid, "old-service");
    assert!(pending[0].error.contains("observation forbidden"));
    assert_eq!(fixture.objects.lock().unwrap().len(), 1);
    assert!(
        !fixture
            .recorded
            .lock()
            .unwrap()
            .iter()
            .any(|(request, _)| request.starts_with("DELETE ") || request.starts_with("PATCH "))
    );
}

#[tokio::test]
async fn terminating_resources_never_bypass_pvc_or_foreign_identity_protection() {
    for case in [Case::UnsafeRetention, Case::OwnedPvc] {
        let fixture = fixture(case).await;
        let snapshot = fixture
            .runtime
            .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
            .await
            .unwrap();
        mark_terminating(&fixture, "StatefulSet");
        let error = fixture
            .runtime
            .retire_agent_remnants(&snapshot)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("volume") || error.to_string().contains("PVC"),
            "{error}"
        );
        assert!(!fixture.recorded.lock().unwrap().iter().any(|(request, _)| request.starts_with("DELETE ") || request.starts_with("PATCH ")));
    }
    for mutation in ["version-only", "replacement", "foreign"] {
        let fixture = fixture(Case::ControllerOnly).await;
        let snapshot = fixture
            .runtime
            .capture_agent_stop("owner", &ServiceType::ComputerAgentRunner)
            .await
            .unwrap();
        if mutation != "version-only" {
            mark_terminating(&fixture, "StatefulSet");
        }
        {
            let mut objects = fixture.objects.lock().unwrap();
            objects[0]["metadata"]["resourceVersion"] = "8".into();
            if mutation == "replacement" {
                objects[0]["metadata"]["uid"] = "replacement-uid".into();
            } else if mutation == "foreign" {
                objects[0]["metadata"]["labels"]["rcoder.io/identifier"] = "foreign".into();
            }
        }
        assert!(matches!(
            fixture.runtime.stop_captured_agent(&snapshot).await,
            Err(Error::Conflict(_))
        ));
        assert!(
            !fixture.recorded.lock().unwrap().iter().any(|(request, _)|
                request.starts_with("DELETE ") || request.starts_with("PATCH "))
        );
    }
}
