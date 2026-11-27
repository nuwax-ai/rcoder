//! Start-path image alignment guard regressions (spec sandbox-image-version-drift R1).
//!
//! Real HTTP/Kubernetes client: the start write only rolls a pod template onto
//! the platform's current image when the template already runs a
//! platform-managed reference (same repository). Foreign or pinned images keep
//! the template and start on it unchanged.
use super::*;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn runtime(client: kube::Client) -> KubernetesRuntime {
    use crate::runtime::kubernetes_runtime::KubernetesRuntimeConfig;
    KubernetesRuntime {
        client,
        namespace: "alignment-test".into(),
        config: KubernetesRuntimeConfig {
            namespace: "alignment-test".into(),
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
        workspace_exists_cache: Default::default(),
        event_publisher: Default::default(),
        event_counters: Arc::new(crate::runtime::k8s_event_publisher::PublisherCounters::default()),
    }
}

fn deployment_fixture(template_image: &str) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {
            "name": "rcoder-app-9",
            "uid": "deploy-uid-1",
            "resourceVersion": "1",
            "labels": {
                "app.kubernetes.io/name": "user-app",
                "app.kubernetes.io/instance": "9",
                "app.kubernetes.io/managed-by": "rcoder-app-manager",
                "app.kubernetes.io/part-of": "rcoder",
                "rcoder.io/app-id": "9"
            },
            "annotations": {
                "rcoder.io/application-id": "9",
                "rcoder.io/lifecycle-id": "life-9"
            }
        },
        "spec": {
            "template": {
                "spec": {"containers": [{"name": "app", "image": template_image}]}
            }
        }
    })
}

fn pvc_fixture(name: &str) -> serde_json::Value {
    let uid = format!("{name}-uid");
    serde_json::json!({
        "apiVersion": "v1",
        "kind": "PersistentVolumeClaim",
        "metadata": {
            "name": name,
            "uid": uid,
            "resourceVersion": "1",
            "labels": {
                "app.kubernetes.io/managed-by": "rcoder-app-manager",
                "service_type": "user-app"
            },
            "annotations": {
                "rcoder.io/application-id": "9",
                "rcoder.io/lifecycle-id": "life-9"
            }
        },
        "spec": {
            "accessModes": ["ReadWriteOnce"],
            "resources": {"requests": {"storage": "1Gi"}}
        }
    })
}

fn captured_target() -> shared_types::UserAppMutationTarget {
    shared_types::UserAppMutationTarget {
        context: shared_types::UserAppExecutionContext {
            app_id: "9".into(),
            lifecycle_id: "life-9".into(),
            operation_id: "start-align".into(),
            executor_id: "worker".into(),
            request_fingerprint: "a".repeat(64),
        },
        resource: shared_types::AppResourceIdentity {
            kind: shared_types::AppResourceKind::Deployment,
            name: "rcoder-app-9".into(),
            uid: "deploy-uid-1".into(),
            resource_version: Some("1".into()),
        },
    }
}

/// One aligned start attempt against a mock API server. Returns the final
/// deployment PATCH bodies (claim writes go to PVCs, not the deployment).
async fn aligned_start_patches(
    template_image: &str,
    desired: Option<&str>,
) -> Vec<serde_json::Value> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(rustls::crypto::ring::default_provider().install_default());
    let runtime = runtime(
        kube::Client::try_from(kube::Config::new(
            format!("http://{address}").parse().unwrap(),
        ))
        .unwrap(),
    );
    let deployment = deployment_fixture(template_image);
    let patches = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let server_patches = patches.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            let (head, offset, length) = loop {
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (k, v) = line.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    break (head, end + 4, length);
                }
            };
            while bytes.len() < offset + length {
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            let method = head.split_whitespace().next().unwrap();
            let path = head
                .split_whitespace()
                .nth(1)
                .unwrap()
                .split('?')
                .next()
                .unwrap();
            let (code, value) = if path.ends_with("/deployments/rcoder-app-9") {
                if method == "PATCH" {
                    let patch: serde_json::Value =
                        serde_json::from_slice(&bytes[offset..offset + length]).unwrap();
                    server_patches.lock().unwrap().push(patch);
                }
                (200, deployment.clone())
            } else if path.ends_with("/persistentvolumeclaims/rcoder-app-9-workspace")
                || path.ends_with("/persistentvolumeclaims/rcoder-app-9-data")
            {
                let name = if path.ends_with("-workspace") {
                    "rcoder-app-9-workspace"
                } else {
                    "rcoder-app-9-data"
                };
                (200, pvc_fixture(name))
            } else {
                (
                    404,
                    serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure",
                        "reason":"NotFound","message":"absent","code":404}),
                )
            };
            let body = value.to_string();
            let response = format!(
                "HTTP/1.1 {code} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let target = captured_target();
    runtime
        .start_captured_with_policy(&target, true, desired)
        .await
        .expect("aligned start");
    server.abort();
    patches.lock().unwrap().clone()
}

#[tokio::test]
async fn start_rolls_platform_managed_template_onto_current_platform_image() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let patches = aligned_start_patches(
            "registry.test/app-runtime:0.1.293",
            Some("registry.test/app-runtime:0.1.450"),
        )
        .await;
        assert_eq!(patches.len(), 1, "one deployment write");
        let containers = patches[0]["spec"]["template"]["spec"]["containers"]
            .as_array()
            .expect("aligned start carries containers");
        assert_eq!(containers[0]["name"], "app");
        assert_eq!(containers[0]["image"], "registry.test/app-runtime:0.1.450");
        assert_eq!(patches[0]["spec"]["replicas"], 1);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn start_keeps_foreign_and_pinned_images_without_rolling() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        for template in ["docker.io/someone/custom:1", "sha256:0123abcd"] {
            let patches =
                aligned_start_patches(template, Some("registry.test/app-runtime:0.1.450")).await;
            assert_eq!(patches.len(), 1, "one deployment write for {template}");
            assert!(
                patches[0]["spec"].get("template").is_none()
                    || patches[0]["spec"]["template"].get("spec").is_none(),
                "foreign template {template} must not be rolled"
            );
            assert_eq!(patches[0]["spec"]["replicas"], 1);
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn start_without_desired_image_never_touches_containers() {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let patches = aligned_start_patches("registry.test/app-runtime:0.1.293", None).await;
        assert_eq!(patches.len(), 1);
        assert!(patches[0]["spec"].get("template").is_none());
    })
    .await
    .unwrap();
}
