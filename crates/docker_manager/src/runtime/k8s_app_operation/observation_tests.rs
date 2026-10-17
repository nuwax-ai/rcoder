//! Draft protocol fixtures. These tests are intentionally not evidence of a
//! compiled/tested implementation until the phase-3 execution gate is opened.

use super::*;
use container_runtime_api::UserAppDeploymentRuntime;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const APP: &str = "wake-app";
const LIFE: &str = "life-one";
const NAMESPACE: &str = "mutex-test";
const NAME: &str = "rcoder-operation-prod-wake-app";

fn lease_json(uid: &str, token: &str, rv: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "coordination.k8s.io/v1", "kind": "Lease",
        "metadata": {
            "namespace": NAMESPACE, "name": NAME,
            "uid": uid, "resourceVersion": rv,
            "labels": {
                "rcoder.io/operation-app": APP,
                "rcoder.io/operation-family": "user-app",
            },
            "annotations": {
                "rcoder.io/lifecycle-id": LIFE,
                "rcoder.io/lease-token": token,
            },
        },
        "spec": { "renewTime": lease_now(), "leaseDurationSeconds": LEASE_TTL_SECONDS },
    })
}

fn original() -> AppOperationMutexIdentity {
    mutex_identity(
        &serde_json::from_value(lease_json("uid-one", "private-token-one", "41")).unwrap(),
        NAMESPACE,
        NAME,
        APP,
        LIFE,
    )
    .unwrap()
}

#[test]
fn identity_requires_exact_scope_labels_uid_token_and_resource_version() {
    for (pointer, value) in [
        (
            "/metadata/namespace",
            serde_json::json!("foreign-namespace"),
        ),
        ("/metadata/name", serde_json::json!("foreign-name")),
        (
            "/metadata/labels/rcoder.io~1operation-app",
            serde_json::json!("foreign-app"),
        ),
        (
            "/metadata/labels/rcoder.io~1operation-family",
            serde_json::json!("user-app-builder"),
        ),
        ("/metadata/uid", serde_json::json!("")),
        ("/metadata/resourceVersion", serde_json::json!("")),
        (
            "/metadata/annotations/rcoder.io~1lease-token",
            serde_json::json!(""),
        ),
    ] {
        let mut object = lease_json("uid-one", "private-token-one", "41");
        *object.pointer_mut(pointer).unwrap() = value;
        let object: Lease = serde_json::from_value(object).unwrap();
        assert!(
            mutex_identity(&object, NAMESPACE, NAME, APP, LIFE).is_err(),
            "{pointer}"
        );
    }
    for field in [
        "uid",
        "resourceVersion",
        "namespace",
        "name",
        "labels",
        "annotations",
    ] {
        let mut object = lease_json("uid-one", "private-token-one", "41");
        object["metadata"].as_object_mut().unwrap().remove(field);
        let object: Lease = serde_json::from_value(object).unwrap();
        assert!(
            mutex_identity(&object, NAMESPACE, NAME, APP, LIFE).is_err(),
            "{field}"
        );
    }
}

#[test]
fn lifecycleless_lease_is_observable_and_debug_redacts_holder_token() {
    let mut object = lease_json("uid-one", "private-token-one", "41");
    object["metadata"]["annotations"]
        .as_object_mut()
        .unwrap()
        .remove("rcoder.io/lifecycle-id");
    let object: Lease = serde_json::from_value(object).unwrap();
    let identity = mutex_identity(&object, NAMESPACE, NAME, APP, LIFE).unwrap();
    assert_eq!(identity.lifecycle_id, None);
    let debug = format!("{:?}", AppOperationMutexObservation::Occupied(identity));
    assert!(!debug.contains("private-token-one"));
    assert!(debug.contains("[redacted]"));
}

#[derive(Clone, Copy)]
enum Endpoint {
    Lease,
    List,
    Watch(&'static str),
}

fn lease_list(resource_version: &str, items: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "coordination.k8s.io/v1", "kind": "LeaseList",
        "metadata": {"resourceVersion": resource_version}, "items": items,
    })
}

enum Reply {
    Json(u16, serde_json::Value),
    Events(Vec<serde_json::Value>),
    Delay(std::time::Duration, Box<Reply>),
    Silent(std::time::Duration),
}

fn status(code: u16) -> serde_json::Value {
    let reason = match code {
        404 => "NotFound",
        410 => "Expired",
        403 => "Forbidden",
        _ => "Failure",
    };
    serde_json::json!({
        "apiVersion": "v1", "kind": "Status", "status": "Failure",
        "reason": reason, "code": code, "message": "fixture status",
    })
}

async fn runtime_for(address: std::net::SocketAddr) -> KubernetesRuntime {
    use crate::runtime::kubernetes_runtime::KubernetesRuntimeConfig;
    drop(rustls::crypto::ring::default_provider().install_default());
    let client = kube::Client::try_from(kube::Config::new(
        format!("http://{address}").parse().unwrap(),
    ))
    .unwrap();
    KubernetesRuntime {
        client,
        namespace: NAMESPACE.into(),
        config: KubernetesRuntimeConfig {
            namespace: NAMESPACE.into(),
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
            execution_authority: "k8s:mutex-fixture".into(),
        },
        pod_cache: Default::default(),
        subvolume_path_cache: Default::default(),
        event_publisher: Default::default(),
        event_counters: Arc::new(crate::runtime::k8s_event_publisher::PublisherCounters::default()),
    }
}

async fn read_head(stream: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let read = stream.read(&mut buffer).await.unwrap();
        assert!(read > 0, "fixture request ended before headers");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            return String::from_utf8(bytes[..end].to_vec()).unwrap();
        }
    }
}

async fn reply(stream: &mut tokio::net::TcpStream, mut response: Reply) {
    // Iterative unwrapping keeps the fixture future non-recursive.
    loop {
        match response {
            Reply::Delay(duration, next) => {
                tokio::time::sleep(duration).await;
                response = *next;
            }
            Reply::Silent(duration) => {
                tokio::time::sleep(duration).await;
                return;
            }
            Reply::Json(code, value) => {
                let body = value.to_string();
                stream.write_all(format!(
                    "HTTP/1.1 {code} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                ).as_bytes()).await.unwrap();
                return;
            }
            Reply::Events(events) => {
                let body: String = events
                    .into_iter()
                    .map(|event| format!("{event}\n"))
                    .collect();
                stream.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                ).as_bytes()).await.unwrap();
                return;
            }
        }
    }
}

async fn fixture(
    steps: Vec<(Endpoint, Reply)>,
) -> (KubernetesRuntime, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for (endpoint, response) in steps {
            let (mut stream, _) = listener.accept().await.unwrap();
            let head = read_head(&mut stream).await;
            // An accidental write is answered before failing, so it cannot
            // disguise a mutation regression as a hanging test.
            if !head.starts_with("GET ") {
                reply(&mut stream, Reply::Json(403, status(403))).await;
                panic!("mutex observation must remain read-only: {head}");
            }
            let line = head.lines().next().unwrap();
            match endpoint {
                Endpoint::Lease => assert!(
                    line.starts_with(&format!(
                        "GET /apis/coordination.k8s.io/v1/namespaces/{NAMESPACE}/leases/{NAME} "
                    )),
                    "{line}"
                ),
                Endpoint::List => {
                    assert!(
                        line.starts_with(&format!(
                            "GET /apis/coordination.k8s.io/v1/namespaces/{NAMESPACE}/leases?"
                        )),
                        "{line}"
                    );
                    assert!(
                        line.contains(&format!("fieldSelector=metadata.name%3D{NAME}")),
                        "{line}"
                    );
                    assert!(!line.contains("watch=true"), "{line}");
                    assert!(!line.contains("labelSelector="), "{line}");
                    assert!(
                        !line.contains("resourceVersion="),
                        "relist requires a fresh collection snapshot: {line}"
                    );
                }
                Endpoint::Watch(rv) => {
                    assert!(
                        line.starts_with(&format!(
                            "GET /apis/coordination.k8s.io/v1/namespaces/{NAMESPACE}/leases?"
                        )),
                        "{line}"
                    );
                    assert!(line.contains("watch=true"), "{line}");
                    assert!(line.contains(&format!("resourceVersion={rv}")), "{line}");
                    assert!(
                        line.contains(&format!("fieldSelector=metadata.name%3D{NAME}")),
                        "{line}"
                    );
                    assert!(
                        !line.contains("labelSelector="),
                        "foreign labels must not be filtered away: {line}"
                    );
                }
            }
            reply(&mut stream, response).await;
        }
        if let Ok(Ok((mut stream, _))) =
            tokio::time::timeout(std::time::Duration::from_millis(30), listener.accept()).await
        {
            let head = read_head(&mut stream).await;
            reply(&mut stream, Reply::Json(403, status(403))).await;
            panic!("unexpected additional mutex request: {head}");
        }
    });
    (runtime_for(address).await, server)
}

#[tokio::test]
async fn watch_410_relist_uses_collection_cursor_when_object_revision_stays_old() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for streaming_error in [false, true] {
            let object = lease_json("uid-one", "private-token-one", "17");
            let mut deleted = object.clone();
            deleted["metadata"]["resourceVersion"] = "91".into();
            let gone = if streaming_error {
                Reply::Events(vec![
                    serde_json::json!({"type":"ERROR", "object":status(410)}),
                ])
            } else {
                Reply::Json(410, status(410))
            };
            let (runtime, server) = fixture(vec![
                (Endpoint::Lease, Reply::Json(200, object.clone())),
                (Endpoint::Watch("17"), gone),
                (
                    Endpoint::List,
                    Reply::Json(200, lease_list("90", vec![object])),
                ),
                (
                    Endpoint::Watch("90"),
                    Reply::Events(vec![
                        serde_json::json!({"type":"DELETED", "object":deleted}),
                    ]),
                ),
                (Endpoint::Lease, Reply::Json(404, status(404))),
            ])
            .await;
            let mut observed = original();
            observed.resource_version = "17".into();
            assert_eq!(
                runtime
                    .wait_app_operation_mutex_change(
                        APP,
                        LIFE,
                        &observed,
                        std::time::Duration::from_secs(1)
                    )
                    .await
                    .unwrap(),
                AppOperationMutexObservation::Absent,
            );
            assert_eq!(
                observed.resource_version, "17",
                "collection cursor cannot replace the CAS identity"
            );
            server.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn native_absence_is_one_read_and_does_not_require_legacy_history() {
    let (runtime, server) = fixture(vec![(Endpoint::Lease, Reply::Json(404, status(404)))]).await;
    assert_eq!(
        runtime
            .observe_app_operation_mutex(APP, LIFE)
            .await
            .unwrap(),
        AppOperationMutexObservation::Absent
    );
    // The strict fixture rejects any second GET, including a legacy-CM query.
    server.await.unwrap();
}

#[tokio::test]
async fn forbidden_or_foreign_mutex_never_becomes_absent_or_legacy_fallback() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut foreign = lease_json("uid-one", "private-token-one", "41");
        foreign["metadata"]["labels"]["rcoder.io/operation-app"] = "foreign-app".into();
        for response in [Reply::Json(403, status(403)), Reply::Json(200, foreign)] {
            let (runtime, server) = fixture(vec![(Endpoint::Lease, response)]).await;
            assert!(
                runtime
                    .observe_app_operation_mutex(APP, LIFE)
                    .await
                    .is_err()
            );
            server.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn old_lifecycle_expired_mutex_is_a_candidate_and_never_absence() {
    let mut object = lease_json("uid-one", "private-token-one", "41");
    object["metadata"]["annotations"]["rcoder.io/lifecycle-id"] = "life-old".into();
    object["spec"]["renewTime"] = "2000-01-01T00:00:00.000000Z".into();
    let (runtime, server) = fixture(vec![(Endpoint::Lease, Reply::Json(200, object))]).await;
    let AppOperationMutexObservation::Expired(identity) = runtime
        .observe_app_operation_mutex(APP, LIFE)
        .await
        .unwrap()
    else {
        panic!("a same-app expired old lifecycle is a cleanup candidate");
    };
    assert_eq!(identity.lifecycle_id.as_deref(), Some("life-old"));
    server.await.unwrap();
}

#[tokio::test]
async fn ttl_without_any_watch_event_requires_a_fresh_get_before_expired() {
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut initial, _) = listener.accept().await.unwrap();
            let head = read_head(&mut initial).await;
            assert!(head.starts_with(&format!(
                "GET /apis/coordination.k8s.io/v1/namespaces/{NAMESPACE}/leases/{NAME} "
            )), "{head}");
            // Timestamp the renewal only when the first GET is actually served.
            // Whole-second TTL=1 gives watch establishment a full two-second
            // live window, independent of fixture/client setup overhead.
            let mut object = lease_json("uid-one", "private-token-one", "41");
            object["spec"]["leaseDurationSeconds"] = 1.into();
            reply(&mut initial, Reply::Json(200, object.clone())).await;
            drop(initial);

            let (mut watch, _) = listener.accept().await.unwrap();
            let head = read_head(&mut watch).await;
            assert!(head.starts_with(&format!(
                "GET /apis/coordination.k8s.io/v1/namespaces/{NAMESPACE}/leases?"
            )), "{head}");
            assert!(head.contains("watch=true"), "{head}");
            assert!(head.contains("resourceVersion=41"), "{head}");
            // Establish a real streaming response, then keep it open without
            // events, EOF, server timeout or a sleeping fixture's reconnect.
            watch.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.unwrap();
            let (mut recheck, _) = tokio::time::timeout(
                std::time::Duration::from_secs(4), listener.accept(),
            ).await.unwrap().unwrap();
            let head = read_head(&mut recheck).await;
            assert!(head.starts_with(&format!(
                "GET /apis/coordination.k8s.io/v1/namespaces/{NAMESPACE}/leases/{NAME} "
            )), "TTL must trigger a fresh exact-name GET while watch is still open: {head}");
            reply(&mut recheck, Reply::Json(200, object)).await;
            // Retain the watch connection until after the confirming GET has
            // returned, so its closure cannot be the client's wake-up source.
            drop(watch);
        });
        let runtime = runtime_for(address).await;
        let started = tokio::time::Instant::now();
        assert_eq!(
            runtime
                .wait_app_operation_mutex_change(
                    APP,
                    LIFE,
                    &original(),
                    std::time::Duration::from_secs(4),
                )
                .await
                .unwrap(),
            AppOperationMutexObservation::Expired(original()),
        );
        assert!(started.elapsed() >= std::time::Duration::from_secs(1));
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn renewal_uid_or_token_change_wakes_as_validated_occupied() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for (uid, token, rv) in [
            ("uid-one", "private-token-one", "42"),
            ("uid-two", "private-token-one", "42"),
            ("uid-one", "private-token-two", "42"),
        ] {
            let next = lease_json(uid, token, rv);
            let expected = mutex_identity(
                &serde_json::from_value(next.clone()).unwrap(),
                NAMESPACE,
                NAME,
                APP,
                LIFE,
            )
            .unwrap();
            let (runtime, server) = fixture(vec![
                (
                    Endpoint::Lease,
                    Reply::Json(200, lease_json("uid-one", "private-token-one", "41")),
                ),
                (
                    Endpoint::Watch("41"),
                    Reply::Events(vec![serde_json::json!({"type":"MODIFIED", "object":next})]),
                ),
            ])
            .await;
            assert_eq!(
                runtime
                    .wait_app_operation_mutex_change(
                        APP,
                        LIFE,
                        &original(),
                        std::time::Duration::from_secs(1)
                    )
                    .await
                    .unwrap(),
                AppOperationMutexObservation::Occupied(expected)
            );
            server.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn deleted_event_requires_fresh_get_and_can_discover_successor_or_absence() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for successor in [false, true] {
            let mut steps = vec![
                (Endpoint::Lease, Reply::Json(200, lease_json("uid-one", "private-token-one", "41"))),
                (Endpoint::Watch("41"), Reply::Events(vec![serde_json::json!({"type":"DELETED", "object":lease_json("uid-one", "private-token-one", "42")})])),
            ];
            let expected = if successor {
                let next = lease_json("uid-two", "private-token-two", "43");
                steps.push((Endpoint::Lease, Reply::Json(200, next.clone())));
                AppOperationMutexObservation::Occupied(mutex_identity(&serde_json::from_value(next).unwrap(), NAMESPACE, NAME, APP, LIFE).unwrap())
            } else {
                steps.push((Endpoint::Lease, Reply::Json(404, status(404))));
                AppOperationMutexObservation::Absent
            };
            let (runtime, server) = fixture(steps).await;
            assert_eq!(runtime.wait_app_operation_mutex_change(APP, LIFE, &original(), std::time::Duration::from_secs(1)).await.unwrap(), expected);
            server.await.unwrap();
        }
    }).await.unwrap();
}

#[tokio::test]
async fn watch_410_relists_and_foreign_successor_stays_an_error() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for foreign in [false, true] {
            let mut next = lease_json("uid-two", "private-token-two", "43");
            if foreign {
                next["metadata"]["labels"]["rcoder.io/operation-app"] = "foreign-app".into();
            }
            let (runtime, server) = fixture(vec![
                (
                    Endpoint::Lease,
                    Reply::Json(200, lease_json("uid-one", "private-token-one", "41")),
                ),
                (
                    Endpoint::Watch("41"),
                    Reply::Events(vec![
                        serde_json::json!({"type":"ERROR", "object":status(410)}),
                    ]),
                ),
                (
                    Endpoint::List,
                    Reply::Json(200, lease_list("90", vec![next])),
                ),
            ])
            .await;
            let result = runtime
                .wait_app_operation_mutex_change(
                    APP,
                    LIFE,
                    &original(),
                    std::time::Duration::from_secs(1),
                )
                .await;
            if foreign {
                assert!(result.is_err());
            } else {
                let AppOperationMutexObservation::Occupied(identity) = result.unwrap() else {
                    panic!("successor remains occupied");
                };
                assert_eq!(identity.uid, "uid-two");
            }
            server.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn one_deadline_covers_watch_410_and_pending_relist() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let budget = std::time::Duration::from_millis(200);
        let (runtime, server) = fixture(vec![
            (
                Endpoint::Lease,
                Reply::Json(200, lease_json("uid-one", "private-token-one", "41")),
            ),
            (
                Endpoint::Watch("41"),
                Reply::Delay(
                    std::time::Duration::from_millis(150),
                    Box::new(Reply::Events(vec![
                        serde_json::json!({"type":"ERROR", "object":status(410)}),
                    ])),
                ),
            ),
            (
                Endpoint::List,
                Reply::Silent(std::time::Duration::from_millis(300)),
            ),
        ])
        .await;
        let start = tokio::time::Instant::now();
        assert_eq!(
            runtime
                .wait_app_operation_mutex_change(APP, LIFE, &original(), budget)
                .await
                .unwrap(),
            AppOperationMutexObservation::Unconfirmed
        );
        assert!(
            start.elapsed() < std::time::Duration::from_millis(300),
            "410 must not reset the 200ms budget"
        );
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn relist_keeps_item_revision_and_empty_list_requires_exact_get() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for empty in [false, true] {
            let mut expired = lease_json("uid-one", "private-token-one", "41");
            expired["spec"]["renewTime"] = "2000-01-01T00:00:00.000000Z".into();
            let mut steps = vec![
                (
                    Endpoint::Lease,
                    Reply::Json(200, lease_json("uid-one", "private-token-one", "41")),
                ),
                (
                    Endpoint::Watch("41"),
                    Reply::Events(vec![
                        serde_json::json!({"type":"ERROR", "object":status(410)}),
                    ]),
                ),
                (
                    Endpoint::List,
                    Reply::Json(
                        200,
                        lease_list("90", if empty { vec![] } else { vec![expired] }),
                    ),
                ),
            ];
            let expected = if empty {
                steps.push((Endpoint::Lease, Reply::Json(404, status(404))));
                AppOperationMutexObservation::Absent
            } else {
                // Collection RV 90 is only a watch cursor, never a CAS anchor.
                AppOperationMutexObservation::Expired(original())
            };
            let (runtime, server) = fixture(steps).await;
            assert_eq!(
                runtime
                    .wait_app_operation_mutex_change(
                        APP,
                        LIFE,
                        &original(),
                        std::time::Duration::from_secs(1)
                    )
                    .await
                    .unwrap(),
                expected
            );
            server.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn relist_rejects_duplicate_incomplete_missing_cursor_or_failed_snapshot() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for variant in ["duplicate", "continue", "missing-cursor", "forbidden"] {
            let object = lease_json("uid-one", "private-token-one", "41");
            let mut snapshot = lease_list("90", vec![object.clone()]);
            match variant {
                "duplicate" => snapshot["items"].as_array_mut().unwrap().push(object),
                "continue" => snapshot["metadata"]["continue"] = "unfinished".into(),
                "missing-cursor" => {
                    snapshot["metadata"]
                        .as_object_mut()
                        .unwrap()
                        .remove("resourceVersion");
                }
                "forbidden" => {}
                _ => unreachable!(),
            }
            let response = if variant == "forbidden" {
                Reply::Json(403, status(403))
            } else {
                Reply::Json(200, snapshot)
            };
            let (runtime, server) = fixture(vec![
                (
                    Endpoint::Lease,
                    Reply::Json(200, lease_json("uid-one", "private-token-one", "41")),
                ),
                (
                    Endpoint::Watch("41"),
                    Reply::Events(vec![
                        serde_json::json!({"type":"ERROR", "object":status(410)}),
                    ]),
                ),
                (Endpoint::List, response),
            ])
            .await;
            assert!(
                runtime
                    .wait_app_operation_mutex_change(
                        APP,
                        LIFE,
                        &original(),
                        std::time::Duration::from_secs(1)
                    )
                    .await
                    .is_err(),
                "{variant}"
            );
            server.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn zero_budget_or_invalid_observation_issues_no_request() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (runtime, server) = fixture(vec![]).await;
        assert_eq!(
            runtime
                .wait_app_operation_mutex_change(APP, LIFE, &original(), std::time::Duration::ZERO)
                .await
                .unwrap(),
            AppOperationMutexObservation::Unconfirmed
        );
        let mut foreign = original();
        foreign.namespace = "foreign".into();
        assert!(
            runtime
                .wait_app_operation_mutex_change(
                    APP,
                    LIFE,
                    &foreign,
                    std::time::Duration::from_secs(1)
                )
                .await
                .is_err()
        );
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn empty_lifecycle_live_lease_is_occupied_without_lifecycle_authority() {
    let mut object = lease_json("uid-one", "private-token-one", "41");
    object["metadata"]["annotations"]["rcoder.io/lifecycle-id"] = "".into();
    let (runtime, server) = fixture(vec![(Endpoint::Lease, Reply::Json(200, object))]).await;
    let AppOperationMutexObservation::Occupied(identity) = runtime
        .observe_app_operation_mutex(APP, LIFE)
        .await
        .unwrap()
    else {
        panic!("an empty optional lifecycle must not hide a live mutex");
    };
    let mut expected = original();
    expected.lifecycle_id = None;
    assert_eq!(identity, expected);
    server.await.unwrap();
}

#[tokio::test]
async fn empty_lifecycle_expired_lease_is_only_an_exact_observation_candidate() {
    let mut object = lease_json("uid-one", "private-token-one", "41");
    object["metadata"]["annotations"]["rcoder.io/lifecycle-id"] = "".into();
    object["spec"]["renewTime"] = "2000-01-01T00:00:00.000000Z".into();
    let (runtime, server) = fixture(vec![(Endpoint::Lease, Reply::Json(200, object))]).await;
    let AppOperationMutexObservation::Expired(identity) = runtime
        .observe_app_operation_mutex(APP, LIFE)
        .await
        .unwrap()
    else {
        panic!("an expired empty-lifecycle mutex is a candidate, never absence");
    };
    let mut expected = original();
    expected.lifecycle_id = None;
    assert_eq!(identity, expected);
    server.await.unwrap();
}

#[test]
fn empty_lifecycle_manual_identity_stays_invalid_and_nonempty_values_are_not_trimmed() {
    let mut manual = original();
    manual.lifecycle_id = Some(String::new());
    assert!(matches!(
        validate_observed_scope(&manual, NAMESPACE, NAME, LIFE),
        Err(ContainerRuntimeError::Conflict(_))
    ));
    for value in [" ", "\t", "life-old"] {
        let mut object = lease_json("uid-one", "private-token-one", "41");
        object["metadata"]["annotations"]["rcoder.io/lifecycle-id"] = value.into();
        let object: Lease = serde_json::from_value(object).unwrap();
        let identity = mutex_identity(&object, NAMESPACE, NAME, APP, LIFE).unwrap();
        assert_eq!(identity.lifecycle_id.as_deref(), Some(value));
    }
}

#[test]
fn empty_lifecycle_never_relaxes_physical_mutex_identity() {
    for pointer in [
        "/metadata/namespace",
        "/metadata/name",
        "/metadata/labels/rcoder.io~1operation-app",
        "/metadata/labels/rcoder.io~1operation-family",
        "/metadata/uid",
        "/metadata/resourceVersion",
        "/metadata/annotations/rcoder.io~1lease-token",
    ] {
        let mut object = lease_json("uid-one", "private-token-one", "41");
        object["metadata"]["annotations"]["rcoder.io/lifecycle-id"] = "".into();
        *object.pointer_mut(pointer).unwrap() = "".into();
        let object: Lease = serde_json::from_value(object).unwrap();
        assert!(
            mutex_identity(&object, NAMESPACE, NAME, APP, LIFE).is_err(),
            "{pointer}"
        );
    }
}
