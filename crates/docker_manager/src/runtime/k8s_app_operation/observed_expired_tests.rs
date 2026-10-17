//! Real HTTP assertions for serving's exact observed-expired acquisition.
use super::*;
use container_runtime_api::{AppOperationMutexIdentity, UserAppDeploymentRuntime};

fn observed() -> AppOperationMutexIdentity {
    AppOperationMutexIdentity {
        namespace: "lease-test".into(),
        name: "rcoder-operation-prod-takeover".into(),
        uid: "old-uid".into(),
        token: "old-operation".into(),
        resource_version: "41".into(),
        lifecycle_id: Some("life-old".into()),
    }
}

fn old_lease() -> serde_json::Value {
    let mut object = lease_json("old-uid", "41", Some("old-operation"), 3600, 2);
    object["metadata"]["annotations"]["rcoder.io/lifecycle-id"] = "life-old".into();
    object
}

async fn reject_additional_request(listener: &tokio::net::TcpListener) {
    if let Ok(Ok((mut stream, _))) =
        tokio::time::timeout(std::time::Duration::from_millis(40), listener.accept()).await
    {
        let (head, _) = read_request(&mut stream).await;
        write_reply(&mut stream, 403, &status(403, "unexpected request")).await;
        panic!("observed-expired must not create/rebase or retry a write: {head}");
    }
}

#[tokio::test]
async fn exact_expired_old_lifecycle_uses_uid_rv_cas_and_standard_receipt_release() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (head, _) = read_request(&mut stream).await;
            assert!(head.starts_with("GET "), "must not POST: {head}");
            write_reply(&mut stream, 200, &old_lease()).await;

            let (mut stream, _) = listener.accept().await.unwrap();
            let (head, body) = read_request(&mut stream).await;
            assert!(head.starts_with("PATCH "), "{head}");
            let patch: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(patch["metadata"]["uid"], "old-uid");
            assert_eq!(patch["metadata"]["resourceVersion"], "41");
            assert_eq!(patch["spec"]["leaseTransitions"], 3);
            assert_eq!(
                patch["metadata"]["labels"]["rcoder.io/operation-app"],
                "takeover"
            );
            assert_eq!(
                patch["metadata"]["labels"]["rcoder.io/operation-family"],
                "user-app"
            );
            assert_eq!(
                patch["metadata"]["annotations"]["rcoder.io/lifecycle-id"],
                "life-current"
            );
            assert!(patch["metadata"]["annotations"]["rcoder.io/operation-id"].is_null());
            let token = patch["metadata"]["annotations"]["rcoder.io/legacy-operation-id"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(!token.is_empty());
            let mut taken = old_lease();
            taken["metadata"]["resourceVersion"] = "42".into();
            for (key, value) in patch["metadata"]["annotations"].as_object().unwrap() {
                if value.is_null() {
                    taken["metadata"]["annotations"]
                        .as_object_mut()
                        .unwrap()
                        .remove(key);
                } else {
                    taken["metadata"]["annotations"][key] = value.clone();
                }
            }
            taken["spec"] = patch["spec"].clone();
            write_reply(&mut stream, 200, &taken).await;

            let (mut stream, _) = listener.accept().await.unwrap();
            let (head, _) = read_request(&mut stream).await;
            assert!(head.starts_with("GET "), "{head}");
            write_reply(&mut stream, 200, &taken).await;

            let (mut stream, _) = listener.accept().await.unwrap();
            let (head, body) = read_request(&mut stream).await;
            assert!(head.starts_with("DELETE "), "{head}");
            let delete: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(delete["preconditions"]["uid"], "old-uid");
            assert_eq!(delete["preconditions"]["resourceVersion"], "42");
            write_reply(
                &mut stream,
                200,
                &serde_json::json!({
                    "apiVersion":"v1", "kind":"Status", "status":"Success", "code":200,
                }),
            )
            .await;
            reject_additional_request(&listener).await;
            token
        });
        let runtime = runtime_for(client_for(address).await);
        let lease = runtime
            .acquire_observed_expired_app_operation("takeover", "life-current", &observed())
            .await
            .unwrap()
            .unwrap();
        let receipt = lease.receipt().unwrap();
        lease.release().await.unwrap();
        let token = server.await.unwrap();
        let shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            uid,
            resource_version,
            token: actual,
            ..
        } = receipt
        else {
            panic!("native lease receipt required");
        };
        assert_eq!(uid, "old-uid");
        assert_eq!(resource_version, "42");
        assert_eq!(actual, token);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn live_renewal_or_changed_uid_token_revision_lifecycle_is_definitive_without_write() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for variant in ["live", "renewed", "uid", "token", "rv", "life", "absent"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (head, _) = read_request(&mut stream).await;
                assert!(head.starts_with("GET "), "{head}");
                let mut object = old_lease();
                match variant {
                    "live" => {
                        object["spec"]["renewTime"] = serde_json::to_value(lease_now()).unwrap()
                    }
                    "renewed" => {
                        object["spec"]["renewTime"] = serde_json::to_value(lease_now()).unwrap();
                        object["metadata"]["resourceVersion"] = "42".into();
                    }
                    "uid" => object["metadata"]["uid"] = "new-uid".into(),
                    "token" => {
                        object["metadata"]["annotations"]["rcoder.io/operation-id"] =
                            "new-token".into()
                    }
                    "rv" => object["metadata"]["resourceVersion"] = "42".into(),
                    "life" => {
                        object["metadata"]["annotations"]["rcoder.io/lifecycle-id"] =
                            "new-life".into()
                    }
                    "absent" => {}
                    _ => unreachable!(),
                }
                if variant == "absent" {
                    write_reply(&mut stream, 404, &status(404, "gone")).await;
                } else {
                    write_reply(&mut stream, 200, &object).await;
                }
                reject_additional_request(&listener).await;
            });
            let result = runtime_for(client_for(address).await)
                .acquire_observed_expired_app_operation("takeover", "life-current", &observed())
                .await;
            assert!(
                matches!(result, Err(ContainerRuntimeError::OperationInProgress(_))),
                "{variant}"
            );
            server.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn foreign_scope_or_deleting_expired_object_refuses_without_patch() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for pointer in [
            "/metadata/namespace",
            "/metadata/name",
            "/metadata/labels/rcoder.io~1operation-app",
            "/metadata/labels/rcoder.io~1operation-family",
            "/metadata/deletionTimestamp",
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (head, _) = read_request(&mut stream).await;
                assert!(head.starts_with("GET "), "{head}");
                let mut object = old_lease();
                if pointer == "/metadata/deletionTimestamp" {
                    object["metadata"]["deletionTimestamp"] = "2026-01-01T00:00:00Z".into();
                } else {
                    *object.pointer_mut(pointer).unwrap() = "foreign".into();
                }
                write_reply(&mut stream, 200, &object).await;
                reject_additional_request(&listener).await;
            });
            let result = runtime_for(client_for(address).await)
                .acquire_observed_expired_app_operation("takeover", "life-current", &observed())
                .await;
            assert!(
                matches!(result, Err(ContainerRuntimeError::Conflict(_))),
                "{pointer}"
            );
            server.await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cas_loser_reports_actual_winner_without_rebased_patch_or_post() {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (head, _) = read_request(&mut stream).await;
            assert!(head.starts_with("GET "), "{head}");
            write_reply(&mut stream, 200, &old_lease()).await;
            let (mut stream, _) = listener.accept().await.unwrap();
            let (head, body) = read_request(&mut stream).await;
            assert!(head.starts_with("PATCH "), "{head}");
            let patch: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(patch["metadata"]["resourceVersion"], "41");
            write_reply(&mut stream, 409, &status(409, "winner changed version")).await;
            let (mut stream, _) = listener.accept().await.unwrap();
            let (head, _) = read_request(&mut stream).await;
            assert!(head.starts_with("GET "), "{head}");
            write_reply(
                &mut stream,
                200,
                &lease_json("old-uid", "42", Some("winner-operation"), 0, 3),
            )
            .await;
            reject_additional_request(&listener).await;
        });
        let result = runtime_for(client_for(address).await)
            .acquire_observed_expired_app_operation("takeover", "life-current", &observed())
            .await;
        let Err(ContainerRuntimeError::OperationInProgress(detail)) = result else {
            panic!("CAS loser must retain definitive occupied result");
        };
        assert_eq!(detail.operation_id.as_deref(), Some("winner-operation"));
        server.await.unwrap();
    })
    .await
    .unwrap();
}
