use super::*;
use container_runtime_api::UserAppDeploymentRuntime as _;
use shared_types::{
    ComputeLeaseInspection, UserAppExecutionContext, UserAppOperationLeaseReceipt,
    UserAppOperationScope,
};

fn contexts() -> (UserAppExecutionContext, UserAppExecutionContext) {
    let current = context_for("243", "current-compute");
    let mut owner = current.clone();
    owner.operation_id = "terminal-ensure".into();
    owner.executor_id = "old-executor".into();
    owner.request_fingerprint = "cd".repeat(32);
    (current, owner)
}

fn ordinary_owner_lease(owner: &UserAppExecutionContext, live: bool) -> serde_json::Value {
    let renew = if live {
        lease_now().0.to_string()
    } else {
        "2000-01-01T00:00:00.000000Z".into()
    };
    serde_json::json!({
        "apiVersion":"coordination.k8s.io/v1", "kind":"Lease",
        "metadata": {
            "namespace":"lease-test", "name":"rcoder-operation-builder-243",
            "uid":"old-lease-uid", "resourceVersion":"42",
            "labels": {
                "rcoder.io/operation-app":owner.app_id,
                "rcoder.io/operation-family":ServiceType::UserappBuilder.to_string(),
            },
            "annotations": {
                "rcoder.io/operation-id":owner.operation_id,
                "rcoder.io/lifecycle-id":owner.lifecycle_id,
                "rcoder.io/executor-id":owner.executor_id,
                "rcoder.io/request-fingerprint":owner.request_fingerprint,
            },
        },
        "spec": {
            "holderIdentity":format!("{}:{}",owner.executor_id,owner.operation_id),
            "leaseDurationSeconds":LEASE_TTL_SECONDS,
            "renewTime":renew,
        },
    })
}

fn old_receipt() -> UserAppOperationLeaseReceipt {
    UserAppOperationLeaseReceipt::Kubernetes {
        service_type: ServiceType::UserappBuilder,
        namespace: "lease-test".into(),
        name: "rcoder-operation-builder-243".into(),
        uid: "old-lease-uid".into(),
        resource_version: "42".into(),
        token: "terminal-ensure".into(),
    }
}

/// A real kube::Client performs the request against the fixture server. An
/// unexpected write is answered and then fails the test, rather than hanging.
async fn serve_observation(
    listener: tokio::net::TcpListener,
    object: serde_json::Value,
    get_code: u16,
) {
    let (mut stream, _) = listener.accept().await.expect("lease observation");
    let (head, _) = read_request(&mut stream).await;
    write_reply(&mut stream, get_code, &object).await;
    assert!(
        head.starts_with("GET "),
        "observation must be read-only: {head}"
    );
    assert!(
        head.contains("/leases/rcoder-operation-builder-243"),
        "{head}"
    );
    if let Ok(Ok((mut stream, _))) =
        tokio::time::timeout(std::time::Duration::from_millis(30), listener.accept()).await
    {
        let (head, _) = read_request(&mut stream).await;
        write_reply(&mut stream, 409, &status(409, "unexpected write")).await;
        panic!("observation or rejected release must not mutate: {head}");
    }
}

#[tokio::test]
async fn previous_owner_candidate_is_read_only_and_preserves_live_foreign_unknown_leases() {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let (current, owner) = contexts();
        for scenario in [
            "expired",
            "live",
            "foreign-app",
            "foreign-family",
            "foreign-life",
            "same-operation",
            "missing-executor",
            "missing-fingerprint",
            "missing-uid",
            "missing-rv",
            "holder",
            "mixed-compute",
            "mixed-token",
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let mut object = ordinary_owner_lease(&owner, scenario == "live");
            match scenario {
                "foreign-app" => {
                    object["metadata"]["labels"]["rcoder.io/operation-app"] = "other-app".into()
                }
                "foreign-family" => {
                    object["metadata"]["labels"]["rcoder.io/operation-family"] =
                        ServiceType::Userapp.to_string().into()
                }
                "foreign-life" => {
                    object["metadata"]["annotations"]["rcoder.io/lifecycle-id"] =
                        "other-life".into()
                }
                "same-operation" => {
                    object["metadata"]["annotations"]["rcoder.io/operation-id"] =
                        current.operation_id.clone().into()
                }
                "missing-executor" => {
                    object["metadata"]["annotations"]
                        .as_object_mut()
                        .unwrap()
                        .remove("rcoder.io/executor-id");
                }
                "missing-fingerprint" => {
                    object["metadata"]["annotations"]
                        .as_object_mut()
                        .unwrap()
                        .remove("rcoder.io/request-fingerprint");
                }
                "missing-uid" => {
                    object["metadata"].as_object_mut().unwrap().remove("uid");
                }
                "missing-rv" => {
                    object["metadata"]
                        .as_object_mut()
                        .unwrap()
                        .remove("resourceVersion");
                }
                "holder" => object["spec"]["holderIdentity"] = "unrelated-holder".into(),
                "mixed-compute" => {
                    object["metadata"]["annotations"]["rcoder.io/compute-lease"] = "true".into()
                }
                "mixed-token" => {
                    object["metadata"]["annotations"]["rcoder.io/lease-token"] =
                        owner.executor_id.clone().into()
                }
                _ => {}
            }
            let server = tokio::spawn(serve_observation(listener, object, 200));
            let result = runtime_for(client_for(address).await)
                .inspect_compute_drain_lease(&current, UserAppOperationScope::Dev, None)
                .await
                .unwrap();
            match scenario {
                "expired" => assert_eq!(
                    result,
                    ComputeLeaseInspection::PreviousOwner {
                        receipt: old_receipt(),
                        owner: owner.clone()
                    }
                ),
                "live" => assert_eq!(result, ComputeLeaseInspection::Held),
                _ => assert!(
                    matches!(result, ComputeLeaseInspection::IdentityChanged(_)),
                    "{scenario}: {result:?}"
                ),
            }
            server.await.expect("read-only protocol assertions");
        }
    })
    .await
    .expect("bounded previous owner observation");
}

#[tokio::test]
async fn previous_owner_release_uses_captured_uid_and_revision_and_preserves_cas_conflict() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let (current, owner) = contexts();
        for delete_code in [200, 404, 409, 500] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let object = ordinary_owner_lease(&owner, false);
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (head, _) = read_request(&mut stream).await;
                assert!(head.starts_with("GET "));
                write_reply(&mut stream, 200, &object).await;
                let (mut stream, _) = listener.accept().await.unwrap();
                let (head, body) = read_request(&mut stream).await;
                assert!(head.starts_with("DELETE "));
                assert!(head.contains("/leases/rcoder-operation-builder-243"));
                let params: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(params["preconditions"]["uid"], "old-lease-uid");
                assert_eq!(params["preconditions"]["resourceVersion"], "42");
                if delete_code == 200 {
                    write_reply(&mut stream, 200, &object).await;
                } else {
                    write_reply(
                        &mut stream,
                        delete_code,
                        &status(delete_code, "release rejected"),
                    )
                    .await;
                }
            });
            let result = runtime_for(client_for(address).await)
                .release_previous_compute_lease(
                    &current,
                    UserAppOperationScope::Dev,
                    &owner,
                    &old_receipt(),
                )
                .await;
            match delete_code {
                200 | 404 => result.expect("exact predecessor released or already absent"),
                409 => assert!(matches!(result, Err(ContainerRuntimeError::Conflict(_)))),
                _ => assert!(
                    result.is_err(),
                    "unknown release result must retain recovery"
                ),
            }
            server
                .await
                .expect("conditional delete protocol assertions");
        }
    })
    .await
    .expect("bounded conditional previous owner release");
}

#[tokio::test]
async fn previous_owner_release_preserves_changed_or_live_lease_and_read_errors() {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let (current, owner) = contexts();
        for scenario in [
            "uid",
            "rv",
            "token",
            "executor",
            "changed-executor",
            "successor",
            "fingerprint",
            "lifecycle",
            "app",
            "family",
            "compute",
            "holder",
            "live",
            "read-error",
            "absent",
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let mut object = ordinary_owner_lease(&owner, scenario == "live");
            match scenario {
                "uid" => object["metadata"]["uid"] = "replacement-uid".into(),
                "rv" => object["metadata"]["resourceVersion"] = "43".into(),
                "token" => {
                    object["metadata"]["annotations"]["rcoder.io/operation-id"] =
                        "replacement-operation".into()
                }
                "executor" => {
                    object["metadata"]["annotations"]["rcoder.io/executor-id"] =
                        "replacement-executor".into()
                }
                "changed-executor" => {
                    object["metadata"]["annotations"]["rcoder.io/executor-id"] =
                        "replacement-executor".into();
                    object["spec"]["holderIdentity"] =
                        format!("replacement-executor:{}", owner.operation_id).into();
                }
                "successor" => {
                    object["metadata"]["annotations"]["rcoder.io/executor-id"] =
                        "replacement-executor".into();
                    object["metadata"]["annotations"]["rcoder.io/operation-id"] =
                        "replacement-operation".into();
                    object["spec"]["holderIdentity"] =
                        "replacement-executor:replacement-operation".into();
                }
                "fingerprint" => {
                    object["metadata"]["annotations"]["rcoder.io/request-fingerprint"] =
                        "ef".repeat(32).into()
                }
                "lifecycle" => {
                    object["metadata"]["annotations"]["rcoder.io/lifecycle-id"] =
                        "replacement-life".into()
                }
                "app" => {
                    object["metadata"]["labels"]["rcoder.io/operation-app"] =
                        "replacement-app".into()
                }
                "family" => {
                    object["metadata"]["labels"]["rcoder.io/operation-family"] =
                        ServiceType::Userapp.to_string().into()
                }
                "compute" => {
                    object["metadata"]["annotations"]["rcoder.io/compute-lease"] = "true".into()
                }
                "holder" => object["spec"]["holderIdentity"] = "replacement-holder".into(),
                "read-error" => object = status(500, "observation unknown"),
                "absent" => object = status(404, "previous owner already released"),
                _ => {}
            }
            let server = tokio::spawn(serve_observation(
                listener,
                object,
                match scenario {
                    "read-error" => 500,
                    "absent" => 404,
                    _ => 200,
                },
            ));
            let result = runtime_for(client_for(address).await)
                .release_previous_compute_lease(
                    &current,
                    UserAppOperationScope::Dev,
                    &owner,
                    &old_receipt(),
                )
                .await;
            if scenario == "absent" {
                result.expect("already absent predecessor release is idempotent");
            } else {
                assert!(
                    result.is_err(),
                    "{scenario}: changed/unknown lease must remain"
                );
            }
            server.await.expect("no-delete protocol assertions");
        }
    })
    .await
    .expect("bounded previous owner identity guards");
}
