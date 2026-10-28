//! Ordinary hot operations bind an already acquired legacy mutex in SQL.
use super::*;
use container_runtime_api::AppOperationLeaseObservation;

#[tokio::test]
async fn bound_legacy_hot_lease_observation_accepts_expiry_without_granting_a_write() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        for variant in [
            "expired",
            "held",
            "optional-matching",
            "optional-empty",
            "matching-operation-token-conflict",
            "uid",
            "token",
            "holder",
            "app",
            "family",
            "life",
            "operation",
            "executor",
            "fingerprint",
            "compute",
            "lease-token",
            "deleting",
            "absent",
            "transport",
        ] {
            let context = context_for("takeover", "original-hot-operation");
            let mut lease = lease_json("original-uid", "99", None, 3600, 0);
            lease["spec"]["holderIdentity"] = "legacy:legacy-holder".into();
            match variant {
                "expired" => {}
                "held" => lease["spec"]["renewTime"] = lease_now().0.to_string().into(),
                "optional-matching" => {
                    for (key, value) in context.resource_metadata() {
                        if key != "rcoder.io/operation-id" {
                            lease["metadata"]["annotations"][key] = value.into();
                        }
                    }
                }
                "optional-empty" => {
                    for key in context.resource_metadata().keys() {
                        if key != "rcoder.io/operation-id" {
                            lease["metadata"]["annotations"][key] = "".into();
                        }
                    }
                }
                "matching-operation-token-conflict" => {
                    lease["metadata"]["annotations"]["rcoder.io/operation-id"] =
                        context.operation_id.clone().into();
                }
                "uid" => lease["metadata"]["uid"] = "successor-uid".into(),
                "token" => {
                    lease["metadata"]["annotations"]["rcoder.io/legacy-operation-id"] =
                        "successor-token".into()
                }
                "holder" => lease["spec"]["holderIdentity"] = "legacy:successor-token".into(),
                "app" => {
                    lease["metadata"]["labels"]["rcoder.io/operation-app"] = "foreign-app".into()
                }
                "family" => {
                    lease["metadata"]["labels"]["rcoder.io/operation-family"] =
                        "user-app-builder".into()
                }
                "life" => {
                    lease["metadata"]["annotations"]["rcoder.io/lifecycle-id"] =
                        "foreign-life".into()
                }
                "operation" => {
                    lease["metadata"]["annotations"]["rcoder.io/operation-id"] =
                        "foreign-operation".into()
                }
                "executor" => {
                    lease["metadata"]["annotations"]["rcoder.io/executor-id"] =
                        "foreign-executor".into()
                }
                "fingerprint" => {
                    lease["metadata"]["annotations"]["rcoder.io/request-fingerprint"] =
                        "cd".repeat(32).into()
                }
                "compute" => {
                    lease["metadata"]["annotations"]["rcoder.io/compute-lease"] = "true".into()
                }
                "lease-token" => {
                    lease["metadata"]["annotations"]["rcoder.io/lease-token"] =
                        context.executor_id.clone().into()
                }
                "deleting" => {
                    lease["metadata"]["deletionTimestamp"] = lease_now().0.to_string().into()
                }
                "absent" | "transport" => {}
                _ => unreachable!(),
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (head, body) = read_request(&mut stream).await;
                assert!(
                    head.starts_with("GET ")
                        && head.contains("/leases/rcoder-operation-prod-takeover"),
                    "{head}"
                );
                assert!(body.is_empty());
                match variant {
                    "absent" => write_reply(&mut stream, 404, &status(404, "gone")).await,
                    "transport" => write_reply(&mut stream, 500, &status(500, "unavailable")).await,
                    _ => write_reply(&mut stream, 200, &lease).await,
                }
                if variant == "absent" {
                    // Lease 缺席后的 legacy ConfigMap fallback：仍为只读 GET。
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let (head, body) = read_request(&mut stream).await;
                    assert!(
                        head.starts_with("GET ")
                            && head.contains("/configmaps/rcoder-operation-prod-takeover"),
                        "fallback must stay read-only: {head}"
                    );
                    assert!(body.is_empty());
                    write_reply(&mut stream, 404, &status(404, "gone")).await;
                }
                observed_expired_tests::reject_additional_request(&listener).await;
            });
            let receipt = shared_types::UserAppOperationLeaseReceipt::Kubernetes {
                service_type: ServiceType::Userapp,
                namespace: "lease-test".into(),
                name: "rcoder-operation-prod-takeover".into(),
                uid: "original-uid".into(),
                resource_version: "old-acquisition-rv".into(),
                token: "legacy-holder".into(),
            };
            let result = runtime_for(client_for(address).await)
                .observe_original_application_lease(&context, &receipt)
                .await;
            match variant {
                "expired" | "optional-matching" | "optional-empty" => assert_eq!(
                    result.unwrap(),
                    AppOperationLeaseObservation::ExpiredOriginalIdentity
                ),
                "held" => assert_eq!(result.unwrap(), AppOperationLeaseObservation::Held),
                "absent" => assert_eq!(
                    result.unwrap(),
                    AppOperationLeaseObservation::AbsentOriginalIdentity
                ),
                _ => assert!(result.is_err(), "{variant}: {result:?}"),
            }
            server.await.unwrap();
        }
    })
    .await
    .expect("bounded read-only legacy hot lease fixture");
}
