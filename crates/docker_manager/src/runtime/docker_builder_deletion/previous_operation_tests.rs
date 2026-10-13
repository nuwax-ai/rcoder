use super::*;
use container_runtime_api::UserAppDeploymentRuntime as _;
use shared_types::AppOperationLease as _;
use std::os::unix::fs::MetadataExt as _;

fn contexts() -> (UserAppExecutionContext, UserAppExecutionContext) {
    let current = UserAppExecutionContext {
        app_id: "previous-ordinary".into(),
        lifecycle_id: "same-life".into(),
        operation_id: "original-compute".into(),
        executor_id: "new-compute-executor".into(),
        request_fingerprint: "ab".repeat(32),
    };
    let mut owner = current.clone();
    owner.operation_id = "old-ordinary".into();
    owner.executor_id = "old-executor".into();
    owner.request_fingerprint = "cd".repeat(32);
    (current, owner)
}

fn runtime() -> DockerRuntime {
    use std::sync::Arc;
    let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
    tokio::spawn(actor.run());
    DockerRuntime::new(Arc::new(crate::DockerManager {
        docker: bollard::Docker::connect_with_http(
            "http://127.0.0.1:9",
            1,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap(),
        config: crate::DockerManagerConfig::default(),
        containers,
        main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
        api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
    }))
}

#[tokio::test]
async fn previous_ordinary_marker_resumes_original_compute_through_runtime_trait() {
    // The child owns a private real lock root, without changing process-global
    // environment while other runtime tests may be executing.
    if std::env::var_os("RCODER_PREVIOUS_OPERATION_TEST_CHILD").is_none() {
        let root = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime::docker_builder_deletion::compute_lease::previous_operation_tests::previous_ordinary_marker_resumes_original_compute_through_runtime_trait",
                "--nocapture",
            ])
            .env("RCODER_PREVIOUS_OPERATION_TEST_CHILD", "1")
            .env("RCODER_OPERATION_RECEIPT_ROOT", root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let runtime = runtime();
    let (current, owner) = contexts();
    for (scope, family, prefix) in [
        (
            UserAppOperationScope::Dev,
            ServiceType::UserappBuilder,
            "builder",
        ),
        (UserAppOperationScope::Prod, ServiceType::Userapp, "prod"),
    ] {
        let lease = runtime
            .acquire_application_file_lease_with_context(&owner.app_id, &family, Some(&owner))
            .await
            .unwrap();
        let receipt = lease.receipt().unwrap();
        let path =
            std::path::PathBuf::from(std::env::var_os("RCODER_OPERATION_RECEIPT_ROOT").unwrap())
                .join(".app-operation-locks")
                .join(format!("{prefix}-{}.lock", current.app_id));
        let before = std::fs::metadata(&path).unwrap();
        assert_eq!(
            runtime
                .inspect_compute_drain_lease(&current, scope, None)
                .await
                .unwrap(),
            ComputeLeaseInspection::Held
        );
        drop(lease); // Actual FD closure leaves only the ordinary operation marker.
        assert_eq!(
            runtime
                .inspect_compute_drain_lease(&current, scope, None)
                .await
                .unwrap(),
            ComputeLeaseInspection::PreviousOperation {
                receipt: receipt.clone(),
                operation_id: owner.operation_id.clone(),
            }
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), owner.operation_id);
        assert!(
            runtime
                .prepare_compute_operation(&current, scope)
                .await
                .is_err()
        );
        for mismatch in [
            "app",
            "lifecycle",
            "operation",
            "family",
            "token",
            "runtime",
        ] {
            let mut wrong_owner = owner.clone();
            let mut wrong_receipt = receipt.clone();
            match mismatch {
                "app" => wrong_owner.app_id = "another-app".into(),
                "lifecycle" => wrong_owner.lifecycle_id = "another-life".into(),
                "operation" => wrong_owner.operation_id = current.operation_id.clone(),
                "family" => {
                    if let UserAppOperationLeaseReceipt::Docker { service_type, .. } =
                        &mut wrong_receipt
                    {
                        *service_type = if family == ServiceType::UserappBuilder {
                            ServiceType::Userapp
                        } else {
                            ServiceType::UserappBuilder
                        };
                    }
                }
                "token" => {
                    if let UserAppOperationLeaseReceipt::Docker { token, .. } = &mut wrong_receipt {
                        *token = "another-operation".into();
                    }
                }
                "runtime" => {
                    wrong_receipt = UserAppOperationLeaseReceipt::Kubernetes {
                        service_type: family,
                        namespace: "test".into(),
                        name: "foreign-runtime".into(),
                        uid: "foreign-uid".into(),
                        resource_version: "1".into(),
                        token: owner.operation_id.clone(),
                    };
                }
                _ => unreachable!(),
            }
            assert!(
                runtime
                    .release_previous_compute_lease(&current, scope, &wrong_owner, &wrong_receipt)
                    .await
                    .is_err(),
                "{mismatch} must not authorize marker cleanup"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), owner.operation_id);
        }
        runtime
            .release_previous_compute_lease(&current, scope, &owner, &receipt)
            .await
            .unwrap();
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        assert!(std::fs::read(&path).unwrap().is_empty());
        // Idempotence observes an already empty marker without rewriting it.
        runtime
            .release_previous_compute_lease(&current, scope, &owner, &receipt)
            .await
            .unwrap();
        let mut prepared = runtime
            .prepare_compute_operation(&current, scope)
            .await
            .unwrap();
        prepared.activate().await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), current.executor_id);
        prepared.release().await.unwrap();
        assert!(std::fs::read(&path).unwrap().is_empty());
    }
}

fn old_receipt(root: &std::path::Path, name: &str) -> UserAppOperationLeaseReceipt {
    let lease = lock_builder_file_with_marker(
        root,
        name,
        shared_types::AppFileMutationMarker::for_operation("old-ordinary").unwrap(),
    )
    .unwrap();
    let receipt = lease.receipt().unwrap();
    drop(lease);
    receipt
}

fn persist_marker(path: &std::path::Path, marker: &[u8]) {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .unwrap();
    file.write_all(marker).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn previous_operation_release_preserves_live_flock_replaced_inode_symlink_and_token() {
    for scenario in ["held", "inode", "symlink", "token"] {
        let root = tempfile::tempdir().unwrap();
        let name = "builder-previous.lock";
        let path = root.path().join(name);
        let receipt = old_receipt(root.path(), name);
        let mut holder = None;
        let mut retained = None;
        match scenario {
            "held" => {
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .unwrap();
                file.try_lock().unwrap();
                holder = Some(file);
                assert_eq!(
                    inspect(&path, ServiceType::UserappBuilder, "current-executor", None).unwrap(),
                    ComputeLeaseInspection::Held
                );
            }
            "inode" | "symlink" => {
                let old_path = root.path().join("retained-original.lock");
                std::fs::rename(&path, &old_path).unwrap();
                if scenario == "inode" {
                    std::fs::write(&path, "replacement-operation").unwrap();
                } else {
                    std::os::unix::fs::symlink(&old_path, &path).unwrap();
                    assert!(matches!(
                        inspect(&path, ServiceType::UserappBuilder, "current-executor", None)
                            .unwrap(),
                        ComputeLeaseInspection::IdentityChanged(_)
                    ));
                }
                retained = Some(old_path);
            }
            "token" => persist_marker(&path, b"replacement-operation"),
            _ => unreachable!(),
        }
        let before = std::fs::read(&path).unwrap();
        assert!(
            release_previous_file_receipt(&path, &receipt).is_err(),
            "{scenario}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before, "{scenario}");
        if let Some(old_path) = retained {
            assert_eq!(std::fs::read_to_string(old_path).unwrap(), "old-ordinary");
        }
        drop(holder);
    }
}

#[test]
fn previous_operation_candidate_rejects_invalid_markers_and_preserves_them() {
    let root = tempfile::tempdir().unwrap();
    let name = "builder-invalid.lock";
    let path = root.path().join(name);
    let receipt = old_receipt(root.path(), name);
    for marker in [
        "../another-operation".to_string(),
        "old\nordinary".into(),
        "x".repeat(65),
    ] {
        persist_marker(&path, marker.as_bytes());
        assert!(matches!(
            inspect(&path, ServiceType::UserappBuilder, "current-executor", None).unwrap(),
            ComputeLeaseInspection::IdentityChanged(_)
        ));
        assert!(release_previous_file_receipt(&path, &receipt).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), marker);
    }
}
