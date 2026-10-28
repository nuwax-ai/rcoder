//! Actual kube HTTP protocol, including an accepted PATCH whose reply is lost.
use super::*;
use container_runtime_api::{ComputeHandoff, UserAppDeploymentRuntime as _};
use shared_types::{ComputeLeaseInspection, UserAppOperationScope};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn compute_handoff_checks_real_renewal_exact_attempt_and_unknown_patch_without_deleting() {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        for scope in [UserAppOperationScope::Dev, UserAppOperationScope::Prod] {
            for scenario in ["live", "expired", "executor", "token", "holder", "missing-optional", "lost-patch", "cas-loser", "stale-attempt", "stale-live"] {
                let context = context_for("handoff", "original");
                let family = shared_types::compute_lease_family(scope).unwrap();
                let name = operation_name(&context.app_id, &family).unwrap();
                let renew = if scenario == "live" || scenario == "stale-live" { lease_now().0.to_string() } else { "2000-01-01T00:00:00.000000Z".into() };
                let mut object = serde_json::json!({
                    "apiVersion":"coordination.k8s.io/v1", "kind":"Lease",
                    "metadata":{"namespace":"lease-test", "name":name, "uid":"original-uid", "resourceVersion":"42",
                        "labels":{"rcoder.io/operation-app":context.app_id, "rcoder.io/operation-family":family.to_string()},
                        "annotations":{"rcoder.io/application-id":context.app_id,"rcoder.io/operation-id":context.operation_id,"rcoder.io/lifecycle-id":context.lifecycle_id,"rcoder.io/executor-id":context.executor_id,"rcoder.io/request-fingerprint":context.request_fingerprint,"rcoder.io/compute-lease":"true","rcoder.io/lease-token":context.executor_id}},
                    "spec":{"holderIdentity":format!("{}:{}",context.executor_id,context.operation_id),"renewTime":renew,"leaseDurationSeconds":LEASE_TTL_SECONDS}
                });
                match scenario {
                    "executor" => object["metadata"]["annotations"]["rcoder.io/executor-id"] = "another-attempt".into(),
                    "token" => object["metadata"]["annotations"]["rcoder.io/lease-token"] = "another-token".into(),
                    "holder" => object["spec"]["holderIdentity"] = "another:original".into(),
                    "missing-optional" => { object["metadata"]["annotations"].as_object_mut().unwrap().remove("rcoder.io/application-id"); }
                    "stale-attempt" | "stale-live" => {
                        object["metadata"]["annotations"]["rcoder.io/executor-id"] = "previous-attempt".into();
                        object["metadata"]["annotations"]["rcoder.io/lease-token"] = "previous-attempt".into();
                        object["spec"]["holderIdentity"] = "previous-attempt:original".into();
                    }
                    _ => {}
                }
                let original = object.clone();
                let stored = Arc::new(Mutex::new(object));
                let patches = Arc::new(AtomicUsize::new(0));
                let deletes = Arc::new(AtomicUsize::new(0));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let (server_stored, server_patches, server_deletes) = (stored.clone(), patches.clone(), deletes.clone());
                let server = tokio::spawn(async move {
                    loop {
                        let (mut stream, _) = listener.accept().await.unwrap();
                        let (head, bytes) = read_request(&mut stream).await;
                        if head.starts_with("GET ") {
                            let object = server_stored.lock().unwrap().clone();
                            write_reply(&mut stream, 200, &object).await;
                        } else if head.starts_with("PATCH ") {
                            server_patches.fetch_add(1, Ordering::SeqCst);
                            let patch: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                            assert_eq!(patch["metadata"]["uid"], "original-uid");
                            assert_eq!(patch["metadata"]["resourceVersion"], "42");
                            assert!(patch["metadata"].get("annotations").is_none(), "retaining must not rekey the original attempt");
                            if scenario == "cas-loser" { write_reply(&mut stream, 409, &status(409, "renewal won")).await; continue; }
                            let updated = {
                                let mut object = server_stored.lock().unwrap();
                                object["spec"]["renewTime"] = patch["spec"]["renewTime"].clone();
                                object["metadata"]["resourceVersion"] = "43".into();
                                object.clone()
                            };
                            if scenario == "lost-patch" { continue; }
                            write_reply(&mut stream, 200, &updated).await;
                        } else {
                            if head.starts_with("DELETE ") { server_deletes.fetch_add(1, Ordering::SeqCst); }
                            write_reply(&mut stream, 409, &status(409, "unexpected write")).await;
                            panic!("handoff must not POST/DELETE: {head}");
                        }
                    }
                });
                let runtime = runtime_for(client_for(address).await);
                let bound = shared_types::UserAppOperationLeaseReceipt::Kubernetes { service_type: family, namespace:"lease-test".into(), name, uid:"original-uid".into(), resource_version:"42".into(), token:context.executor_id.clone() };
                let result = runtime.prepare_compute_handoff(&context, scope, (scenario == "missing-optional").then_some(&bound)).await;
                match scenario {
                    "live" | "cas-loser" | "stale-live" => assert!(matches!(result.unwrap(), ComputeHandoff::Held)),
                    "executor" | "token" | "holder" => assert!(matches!(result.unwrap(), ComputeHandoff::Deferred(observed) if matches!(*observed, ComputeLeaseInspection::IdentityChanged(_)))),
                    "lost-patch" => assert!(result.is_err(), "unknown PATCH cannot become permission to revoke"),
                    "stale-attempt" => {
                        let ComputeHandoff::Deferred(observed) = result.unwrap() else { panic!("stale original attempt requires store fencing proof"); };
                        let ComputeLeaseInspection::Discovered { attempt, .. } = *observed else { panic!("stale original attempt requires its actual identity"); };
                        assert_eq!(attempt.operation_id, context.operation_id);
                        assert_eq!(attempt.executor_id, "previous-attempt");
                        assert_eq!(patches.load(Ordering::SeqCst), 0);
                    }
                    "expired" | "missing-optional" => {
                        let ComputeHandoff::Acquired { guard, original_absent } = result.unwrap() else { panic!("exact expired original can be retained"); };
                        assert!(!original_absent);
                        drop(guard); // cancellation/CAS loss does not delete the original object
                    }
                    _ => unreachable!(),
                }
                assert_eq!(deletes.load(Ordering::SeqCst), 0);
                let observed = stored.lock().unwrap().clone();
                assert_eq!(observed["metadata"]["uid"], original["metadata"]["uid"]);
                assert_eq!(observed["metadata"]["annotations"], original["metadata"]["annotations"]);
                assert_eq!(observed["spec"]["holderIdentity"], original["spec"]["holderIdentity"]);
                if ["live", "executor", "token", "holder", "stale-attempt", "stale-live"].contains(&scenario) { assert_eq!(patches.load(Ordering::SeqCst), 0); assert_eq!(observed, original); }
                server.abort();
            }
        }
    }).await.expect("bounded native Lease handoff protocol");
}
