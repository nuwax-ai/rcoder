use super::*;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn runtime_for(client: kube::Client) -> KubernetesRuntime {
    use crate::runtime::kubernetes_runtime::{KubernetesRuntime, KubernetesRuntimeConfig};
    KubernetesRuntime {
        client,
        namespace: "lease-test".into(),
        config: KubernetesRuntimeConfig {
            namespace: "lease-test".into(),
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
        event_counters: Arc::new(crate::runtime::k8s_event_publisher::PublisherCounters::default()),
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    let (head, offset, length) = loop {
        let n = stream.read(&mut buffer).await.expect("read");
        assert!(n > 0);
        bytes.extend_from_slice(&buffer[..n]);
        if let Some(offset) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..offset]).to_string();
            let length = head
                .lines()
                .find_map(|line| {
                    line.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().expect("length"))
                })
                .unwrap_or(0);
            break (head, offset + 4, length);
        }
    };
    while bytes.len() < offset + length {
        let n = stream.read(&mut buffer).await.expect("body");
        assert!(n > 0);
        bytes.extend_from_slice(&buffer[..n]);
    }
    (head, bytes[offset..offset + length].to_vec())
}

async fn write_reply(stream: &mut tokio::net::TcpStream, code: u16, body: &serde_json::Value) {
    let body = body.to_string();
    stream
            .write_all(
                format!(
                    "HTTP/1.1 {code} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .expect("response");
}

fn status(code: u16, message: &str) -> serde_json::Value {
    let reason = if code == 404 { "NotFound" } else { "Conflict" };
    serde_json::json!({
        "apiVersion": "v1", "kind": "Status", "status": "Failure",
        "reason": reason, "code": code, "message": message,
    })
}

/// 构造一个 Lease JSON。renew 偏移秒数（相对 now）决定过期判定。
fn lease_json(
    uid: &str,
    rv: &str,
    operation_id: Option<&str>,
    renew_age_secs: i64,
    transitions: i32,
) -> serde_json::Value {
    let renew = (k8s_openapi::jiff::Timestamp::now()
        - k8s_openapi::jiff::SignedDuration::from_secs(renew_age_secs))
    .to_string();
    let mut annotations = serde_json::Map::new();
    if let Some(operation) = operation_id {
        annotations.insert("rcoder.io/operation-id".into(), operation.into());
    } else {
        annotations.insert(
            "rcoder.io/legacy-operation-id".into(),
            "legacy-holder".into(),
        );
    }
    serde_json::json!({
        "apiVersion": "coordination.k8s.io/v1", "kind": "Lease",
        "metadata": {
            "name": "rcoder-operation-prod-takeover",
            "namespace": "lease-test",
            "uid": uid, "resourceVersion": rv,
            "labels": {
                "rcoder.io/operation-app": "takeover",
                "rcoder.io/operation-family": ServiceType::Userapp.to_string(),
            },
            "annotations": annotations,
        },
        "spec": {
            "holderIdentity": format!("executor:{operation_id:?}"),
            "leaseDurationSeconds": LEASE_TTL_SECONDS,
            "acquireTime": renew,
            "renewTime": renew,
            "leaseTransitions": transitions,
        },
    })
}

fn context_for(app_id: &str, operation_id: &str) -> shared_types::UserAppExecutionContext {
    shared_types::UserAppExecutionContext {
        app_id: app_id.into(),
        lifecycle_id: "lifecycle-one".into(),
        operation_id: operation_id.into(),
        executor_id: "executor-live".into(),
        request_fingerprint: "ab".repeat(32),
    }
}

async fn client_for(address: std::net::SocketAddr) -> kube::Client {
    drop(rustls::crypto::ring::default_provider().install_default());
    kube::Client::try_from(kube::Config::new(
        format!("http://{address}").parse().expect("uri"),
    ))
    .expect("client")
}

/// 过期租约被 CAS 接管：POST 409 → GET（过期持有者）→ PATCH（断言 RV
/// 锚 + 身份改写 + transitions 递增）→ 获得租约；release 再走 GET→
/// DELETE（实时 RV precondition）。修复前语义（409 一律 OperationInProgress、
/// 永不接管）正是"死持有者永久占锁"事故的根源。
#[tokio::test]
async fn expired_lease_is_taken_over_with_cas_and_releases() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("acquire POST");
            let (head, _) = read_request(&mut stream).await;
            assert!(
                head.starts_with("POST ") && head.contains("/leases"),
                "{head}"
            );
            write_reply(&mut stream, 409, &status(409, "lease exists")).await;

            let (mut stream, _) = listener.accept().await.expect("takeover GET");
            let (head, _) = read_request(&mut stream).await;
            assert!(head.starts_with("GET "), "{head}");
            write_reply(
                &mut stream,
                200,
                &lease_json("dead-holder", "9", Some("dead-operation"), 3600, 2),
            )
            .await;

            let (mut stream, _) = listener.accept().await.expect("takeover PATCH");
            let (head, body) = read_request(&mut stream).await;
            assert!(head.starts_with("PATCH "), "{head}");
            let patch: serde_json::Value = serde_json::from_slice(&body).expect("patch");
            assert_eq!(patch["metadata"]["resourceVersion"], "9", "CAS anchor");
            assert_eq!(patch["spec"]["leaseTransitions"], 3, "transitions bump");
            assert_eq!(
                patch["metadata"]["annotations"]["rcoder.io/operation-id"], "live-operation",
                "holder identity rewrite"
            );
            let mut taken = lease_json("dead-holder", "10", Some("live-operation"), 0, 3);
            taken["metadata"]["resourceVersion"] = "10".into();
            write_reply(&mut stream, 200, &taken).await;

            // release：GET（实时身份/RV）→ DELETE（uid + 实时 RV）。
            let (mut stream, _) = listener.accept().await.expect("release GET");
            let (head, _) = read_request(&mut stream).await;
            assert!(head.starts_with("GET "), "{head}");
            write_reply(&mut stream, 200, &taken).await;

            let (mut stream, _) = listener.accept().await.expect("release DELETE");
            let (head, body) = read_request(&mut stream).await;
            assert!(head.starts_with("DELETE "), "{head}");
            let preconditions: serde_json::Value =
                serde_json::from_slice(&body).expect("release body");
            assert_eq!(preconditions["preconditions"]["uid"], "dead-holder");
            assert_eq!(preconditions["preconditions"]["resourceVersion"], "10");
            write_reply(
                &mut stream,
                200,
                &serde_json::json!({"apiVersion":"v1","kind":"Status",
                        "status":"Success","code":200}),
            )
            .await;
        });
        let context = context_for("takeover", "live-operation");
        let lease = runtime_for(client_for(address).await)
            .acquire_application_operation_with_context(
                "takeover",
                &ServiceType::Userapp,
                Some(&context),
            )
            .await
            .expect("expired lease must be taken over");
        assert!(lease.receipt().is_some());
        lease.release().await.expect("release after takeover");
        server.await.expect("adapter assertions");
    })
    .await
    .expect("takeover scenario within budget");
}

/// 接管竞争失败（PATCH 409 = 别的副本先赢）：上报持有者身份的
/// OperationInProgress，绝不重试抢占。
#[tokio::test]
async fn takeover_race_surfaces_holder_in_progress() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("acquire POST");
            let (head, _) = read_request(&mut stream).await;
            assert!(head.starts_with("POST "), "{head}");
            write_reply(&mut stream, 409, &status(409, "lease exists")).await;

            let (mut stream, _) = listener.accept().await.expect("takeover GET");
            read_request(&mut stream).await;
            write_reply(
                &mut stream,
                200,
                &lease_json("dead-holder", "9", Some("dead-operation"), 3600, 2),
            )
            .await;

            let (mut stream, _) = listener.accept().await.expect("takeover PATCH");
            read_request(&mut stream).await;
            write_reply(&mut stream, 409, &status(409, "another taker won")).await;

            // 竞争失败后重读一次：上报真赢家身份，而非刚读到的死持有者。
            let (mut stream, _) = listener.accept().await.expect("winner re-read");
            read_request(&mut stream).await;
            write_reply(
                &mut stream,
                200,
                &lease_json("actual-winner", "11", Some("winning-operation"), 0, 4),
            )
            .await;
        });
        let context = context_for("takeover", "live-operation");
        let result = runtime_for(client_for(address).await)
            .acquire_application_operation_with_context(
                "takeover",
                &ServiceType::Userapp,
                Some(&context),
            )
            .await;
        assert!(
            matches!(
                &result,
                Err(ContainerRuntimeError::OperationInProgress(operation))
                    if operation.operation_id.as_deref() == Some("winning-operation")
            ),
            "racing takeover must surface the actual winner's identity"
        );
        server.await.expect("adapter assertions");
    })
    .await
    .expect("takeover race within budget");
}

/// 迁移回退：无 Lease 对象时按 legacy ConfigMap 校验并释放（存量 PG
/// 绑定里的旧 receipt 逐字节兼容）。
#[tokio::test]
async fn captured_release_falls_back_to_legacy_configmap() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            // 先 Lease 探测 → 404。
            let (mut stream, _) = listener.accept().await.expect("lease probe");
            let (head, _) = read_request(&mut stream).await;
            assert!(
                head.contains("/leases/rcoder-operation-prod-legacy"),
                "{head}"
            );
            write_reply(&mut stream, 404, &status(404, "not found")).await;

            // ConfigMap GET → 存量身份。
            let (mut stream, _) = listener.accept().await.expect("configmap read");
            let (head, _) = read_request(&mut stream).await;
            assert!(
                head.contains("/configmaps/rcoder-operation-prod-legacy"),
                "{head}"
            );
            write_reply(
                &mut stream,
                200,
                &serde_json::json!({
                    "apiVersion": "v1", "kind": "ConfigMap",
                    "metadata": {
                        "name": "rcoder-operation-prod-legacy",
                        "namespace": "lease-test",
                        "uid": "legacy-uid", "resourceVersion": "5",
                        "labels": {
                            "rcoder.io/operation-app": "legacy",
                            "rcoder.io/operation-family": ServiceType::Userapp.to_string(),
                        },
                        "annotations": {"rcoder.io/legacy-operation-id": "legacy-token"},
                    },
                }),
            )
            .await;

            let (mut stream, _) = listener.accept().await.expect("configmap delete");
            let (head, body) = read_request(&mut stream).await;
            assert!(head.starts_with("DELETE "), "{head}");
            let preconditions: serde_json::Value =
                serde_json::from_slice(&body).expect("release body");
            assert_eq!(preconditions["preconditions"]["uid"], "legacy-uid");
            assert_eq!(preconditions["preconditions"]["resourceVersion"], "5");
            write_reply(
                &mut stream,
                200,
                &serde_json::json!({"apiVersion":"v1","kind":"Status",
                        "status":"Success","code":200}),
            )
            .await;
        });
        let context = context_for("legacy", "legacy-operation");
        let receipt = shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            service_type: ServiceType::Userapp,
            namespace: "lease-test".into(),
            name: "rcoder-operation-prod-legacy".into(),
            uid: "legacy-uid".into(),
            resource_version: "5".into(),
            token: "legacy-token".into(),
        };
        runtime_for(client_for(address).await)
            .release_captured_application_operation(&context, &receipt)
            .await
            .expect("legacy fallback release");
        server.await.expect("adapter assertions");
    })
    .await
    .expect("legacy fallback within budget");
}

/// 释放时的身份防线（D2 语义更新）：Lease 已被别的持有者接管（uid 不符）
/// → 对旧回执是**已释放态**（Ok，清理链可继续 forget），但后继者的锁
/// 绝不被删——服务端必须只观察到一次 GET，任何 DELETE 都会让 release
/// 失败。修复前这里是 Err，喂进清理链造成永久重试与 completed→fence。
#[tokio::test]
async fn captured_release_treats_taken_over_lease_as_released_without_deleting() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("lease probe");
            let (head, _) = read_request(&mut stream).await;
            assert!(
                head.starts_with("GET "),
                "taken-over lease must only be observed, never deleted: {head}"
            );
            write_reply(
                &mut stream,
                200,
                &lease_json("someone-else", "12", Some("other-operation"), 0, 4),
            )
            .await;
        });
        let context = context_for("takeover", "live-operation");
        let receipt = shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            service_type: ServiceType::Userapp,
            namespace: "lease-test".into(),
            name: "rcoder-operation-prod-takeover".into(),
            uid: "dead-holder".into(),
            resource_version: "10".into(),
            token: "live-operation".into(),
        };
        runtime_for(client_for(address).await)
            .release_captured_application_operation(&context, &receipt)
            .await
            .expect("taken-over lease is a released state for the old receipt");
        server.await.expect("adapter assertions");
    })
    .await
    .expect("taken-over release within budget");
}

#[tokio::test]
async fn compute_lease_lost_create_reply_is_discoverable_and_late_release_spares_successor() {
    use container_runtime_api::UserAppDeploymentRuntime as _;
    use shared_types::{ComputeLeaseInspection, UserAppExecutionContext, UserAppOperationScope};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let deletes = Arc::new(AtomicUsize::new(0));
        let observed_deletes = deletes.clone();
        let expire = Arc::new(AtomicBool::new(false));
        let server_expire = expire.clone();
        let server = tokio::spawn(async move {
            let mut stored: Option<serde_json::Value> = None;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (head, body) = read_request(&mut stream).await;
                if head.starts_with("POST ") {
                    if stored.is_some() {
                        write_reply(&mut stream, 409, &status(409, "lease exists")).await;
                        continue;
                    }
                    let mut object: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    object["metadata"]["uid"] = "same-lease-uid".into();
                    object["metadata"]["resourceVersion"] = "1".into();
                    assert_eq!(object["metadata"]["annotations"]["rcoder.io/compute-lease"], "true");
                    stored = Some(object);
                    // The first create commits, but the client never gets its reply.
                } else if head.starts_with("GET ") {
                    match &mut stored {
                        Some(object) => {
                            if server_expire.load(Ordering::SeqCst) {
                                object["spec"]["renewTime"] = "2000-01-01T00:00:00.000000Z".into();
                            }
                            write_reply(&mut stream, 200, object).await;
                        }
                        None => write_reply(&mut stream, 404, &status(404, "absent")).await,
                    }
                } else if head.starts_with("PATCH ") {
                    let patch: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    let object = stored.as_mut().unwrap();
                    assert_eq!(patch["metadata"]["resourceVersion"], object["metadata"]["resourceVersion"]);
                    let annotations = object["metadata"]["annotations"].as_object_mut().unwrap();
                    for (key, value) in patch["metadata"]["annotations"].as_object().unwrap() {
                        if value.is_null() {
                            annotations.remove(key);
                        } else {
                            annotations.insert(key.clone(), value.clone());
                        }
                    }
                    object["metadata"]["labels"] = patch["metadata"]["labels"].clone();
                    object["metadata"]["resourceVersion"] = "2".into();
                    object["spec"] = patch["spec"].clone();
                    write_reply(&mut stream, 200, object).await;
                } else if head.starts_with("DELETE ") {
                    let params: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    let object = stored.take().unwrap();
                    assert_eq!(params["preconditions"]["uid"], object["metadata"]["uid"]);
                    assert_eq!(params["preconditions"]["resourceVersion"], object["metadata"]["resourceVersion"]);
                    observed_deletes.fetch_add(1, Ordering::SeqCst);
                    write_reply(&mut stream, 200, &object).await;
                } else {
                    panic!("unexpected request: {head}");
                }
            }
        });
        let runtime = runtime_for(client_for(address).await);
        let context = UserAppExecutionContext {
            app_id: "computelease".into(),
            lifecycle_id: "life".into(),
            operation_id: "sameoperation".into(),
            executor_id: "firstattempt".into(),
            request_fingerprint: "a".repeat(64),
        };
        assert!(runtime.prepare_compute_operation(&context, UserAppOperationScope::Prod).await.is_err());
        let ComputeLeaseInspection::Discovered { receipt: original, attempt } = runtime
            .inspect_compute_drain_lease(&context, UserAppOperationScope::Prod, None)
            .await.unwrap()
        else { panic!("lost reply receipt must be recovered"); };
        assert_eq!(attempt, context);
        let mut next_context = context.clone();
        next_context.executor_id = "secondattempt".into();
        assert!(matches!(
            runtime.inspect_compute_drain_lease(&next_context, UserAppOperationScope::Prod, None).await.unwrap(),
            ComputeLeaseInspection::Discovered { attempt, .. } if attempt == context
        ));
        let mut foreign = context.clone();
        foreign.request_fingerprint = "b".repeat(64);
        assert!(matches!(
            runtime.inspect_compute_drain_lease(&foreign, UserAppOperationScope::Prod, None).await.unwrap(),
            ComputeLeaseInspection::IdentityChanged(_)
        ));
        // A competing acquisition can take over the very same Lease object.
        // UID alone must not authorize a late release from the previous attempt.
        expire.store(true, Ordering::SeqCst);
        let next = runtime.prepare_compute_operation(&next_context, UserAppOperationScope::Prod).await.unwrap();
        expire.store(false, Ordering::SeqCst);
        let receipt = next.receipt().unwrap();
        match (&original, &receipt) {
            (shared_types::UserAppOperationLeaseReceipt::Kubernetes { uid: old_uid, token: old_token, .. },
             shared_types::UserAppOperationLeaseReceipt::Kubernetes { uid, token, .. }) => {
                assert_eq!(uid, old_uid);
                assert_ne!(token, old_token);
                assert_eq!(token, &next_context.executor_id);
            }
            _ => panic!("Kubernetes receipts required"),
        }
        assert!(matches!(
            runtime.inspect_compute_drain_lease(&context, UserAppOperationScope::Prod, Some(&original)).await.unwrap(),
            ComputeLeaseInspection::IdentityChanged(_)
        ));
        runtime.release_app_operation_receipt(&context, &original).await.unwrap();
        assert_eq!(deletes.load(Ordering::SeqCst), 0);
        next.release().await.unwrap();
        assert_eq!(deletes.load(Ordering::SeqCst), 1);
        server.abort();
    }).await.expect("bounded compute lease protocol test");
}
