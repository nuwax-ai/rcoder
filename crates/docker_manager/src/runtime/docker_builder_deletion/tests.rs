use super::*;
use shared_types::AppOperationLease;

#[cfg(all(unix, feature = "deploy-host"))]
#[tokio::test]
async fn captured_release_uses_host_root_and_allows_reacquisition() {
    // A child process keeps environment changes out of concurrent async tests.
    if std::env::var_os("RCODER_LEASE_ROOT_TEST_CHILD").is_none() {
        let root = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "runtime::docker_builder_deletion::tests::captured_release_uses_host_root_and_allows_reacquisition", "--nocapture"])
            .env("RCODER_LEASE_ROOT_TEST_CHILD", "1")
            .env("RCODER_OPERATION_LOCK_ROOT", root.path())
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let runtime = lease_test_runtime();
    let context = shared_types::UserAppExecutionContext {
        app_id: format!("lease{}", uuid::Uuid::new_v4().simple()),
        lifecycle_id: "life".into(),
        operation_id: "operation".into(),
        executor_id: "executor".into(),
        request_fingerprint: "a".repeat(64),
    };
    for (family, prefix) in [
        (ServiceType::UserappBuilder, "builder"),
        (ServiceType::Userapp, "prod"),
    ] {
        let lease = runtime
            .acquire_application_file_lease(&context.app_id, &family)
            .await
            .unwrap();
        let receipt = lease.receipt().unwrap();
        let path =
            std::path::PathBuf::from(std::env::var_os("RCODER_OPERATION_LOCK_ROOT").unwrap())
                .join(".app-operation-locks")
                .join(format!("{prefix}-{}.lock", context.app_id));
        drop(lease); // death releases the flock, not the durable marker
        assert!(std::fs::metadata(&path).unwrap().len() > 0);
        assert!(
            runtime
                .acquire_application_file_lease(&context.app_id, &family)
                .await
                .is_err()
        );
        runtime
            .release_captured_file_lease(&context, &receipt)
            .await
            .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        runtime
            .acquire_application_file_lease(&context.app_id, &family)
            .await
            .unwrap()
            .release()
            .await
            .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn prepared_compute_file_lease_survives_both_crash_windows() {
    use shared_types::{ComputeLeaseInspection, PreparedComputeLease as _};
    let dir = tempfile::tempdir().unwrap();
    let name = "builder-prepared.lock";
    let path = dir.path().join(name);
    let prepared = prepare_builder_file(
        dir.path(),
        name,
        shared_types::AppFileMutationMarker::for_operation("attemptone").unwrap(),
    )
    .unwrap();
    let receipt = prepared.receipt().unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    assert_eq!(
        compute_lease::inspect(&path, ServiceType::UserappBuilder, "attemptone", None).unwrap(),
        ComputeLeaseInspection::Held
    );
    drop(prepared); // acquired, not bound: no dirty marker
    assert_eq!(
        compute_lease::inspect(&path, ServiceType::UserappBuilder, "attemptone", None).unwrap(),
        ComputeLeaseInspection::Absent
    );
    let mut prepared = prepare_builder_file(
        dir.path(),
        name,
        shared_types::AppFileMutationMarker::for_operation("attemptone").unwrap(),
    )
    .unwrap();
    prepared.activate().await.unwrap(); // DB bind precedes this call in coordinator
    drop(prepared); // bound + activated, before Stopping
    assert_eq!(
        compute_lease::inspect(
            &path,
            ServiceType::UserappBuilder,
            "attemptone",
            Some(&receipt)
        )
        .unwrap(),
        ComputeLeaseInspection::Releasable(receipt.clone())
    );
    release_file_receipt(&path, &receipt).unwrap();
    let mut next = prepare_builder_file(
        dir.path(),
        name,
        shared_types::AppFileMutationMarker::for_operation("attempttwo").unwrap(),
    )
    .unwrap();
    next.activate().await.unwrap();
    drop(next);
    assert!(matches!(
        compute_lease::inspect(
            &path,
            ServiceType::UserappBuilder,
            "attemptone",
            Some(&receipt)
        )
        .unwrap(),
        ComputeLeaseInspection::IdentityChanged(_)
    ));
    assert!(release_file_receipt(&path, &receipt).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "attempttwo");
}

#[cfg(all(unix, feature = "deploy-host"))]
fn lease_test_runtime() -> DockerRuntime {
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

#[cfg(unix)]
#[test]
fn captured_file_release_requires_inactive_original_inode_and_owner() {
    let root = tempfile::tempdir().expect("fixture");
    let path = root.path().join("builder-app.lock");
    let lease = lock_builder_file_with_marker(
        root.path(),
        "builder-app.lock",
        shared_types::AppFileMutationMarker::for_operation("original").expect("marker"),
    )
    .expect("lease");
    let receipt = lease.receipt().expect("receipt");
    assert!(
        release_file_receipt(&path, &receipt).is_err(),
        "live lease cannot be reclaimed"
    );
    assert_eq!(std::fs::read_to_string(&path).expect("marker"), "original");
    drop(lease);
    release_file_receipt(&path, &receipt).expect("completed orphan release");
    release_file_receipt(&path, &receipt).expect("idempotent same inode release");
    assert!(std::fs::read(&path).expect("empty marker").is_empty());
    let next = lock_builder_file_with_marker(
        root.path(),
        "builder-app.lock",
        shared_types::AppFileMutationMarker::for_operation("replacement-owner").expect("marker"),
    )
    .expect("next owner");
    drop(next);
    assert!(release_file_receipt(&path, &receipt).is_err());
    assert_eq!(
        std::fs::read_to_string(&path).expect("replacement marker"),
        "replacement-owner"
    );
    std::fs::rename(&path, root.path().join("retained-original-inode")).expect("keep inode alive");
    std::fs::write(&path, "new-physical-file").expect("replacement");
    assert!(release_file_receipt(&path, &receipt).is_err());
    assert_eq!(
        std::fs::read_to_string(&path).expect("replacement file"),
        "new-physical-file"
    );
}

#[cfg(unix)]
#[test]
fn captured_file_release_rejects_symbolic_link_even_to_original_inode() {
    let root = tempfile::tempdir().expect("fixture");
    let path = root.path().join("builder-app.lock");
    let retained = root.path().join("retained");
    let lease = lock_builder_file_with_marker(
        root.path(),
        "builder-app.lock",
        shared_types::AppFileMutationMarker::for_operation("original").expect("marker"),
    )
    .expect("lease");
    let receipt = lease.receipt().expect("receipt");
    drop(lease);
    std::fs::rename(&path, &retained).expect("rename");
    std::os::unix::fs::symlink(&retained, &path).expect("alias");
    assert!(release_file_receipt(&path, &receipt).is_err());
    assert_eq!(
        std::fs::read_to_string(&retained).expect("original marker"),
        "original"
    );
}

/// D1 反例：默认推导（`!validate`）在 Docker flock 极性下两行判反——
/// 活锁时 validate 返回 Err(Conflict "remains active")，推导成「已死」；
/// 孤儿 marker 时 validate 返回 Ok(true)（authority 残留），推导成「存活」。
/// 本测试以 flock 为活性真源锁死极性。
#[cfg(unix)]
#[test]
fn file_receipt_holder_dead_reads_flock_liveness_not_marker_authority() {
    let root = tempfile::tempdir().expect("fixture");
    let path = root.path().join("builder-app.lock");
    let lease = lock_builder_file_with_marker(
        root.path(),
        "builder-app.lock",
        shared_types::AppFileMutationMarker::for_operation("original").expect("marker"),
    )
    .expect("lease");
    let receipt = lease.receipt().expect("receipt");
    // 活 flock：持有者可能仍在变更，绝不判定已死。
    assert!(!file_receipt_holder_dead(&path, &receipt).expect("live holder probe"));
    // 孤儿 marker + 空闲 flock：内核在进程死亡时释放 flock，marker 只是
    // authority 残留——持有者确定已死。
    drop(lease);
    assert_eq!(
        std::fs::read_to_string(&path).expect("orphan marker"),
        "original"
    );
    assert!(file_receipt_holder_dead(&path, &receipt).expect("orphan holder probe"));
    // 身份被替换：superseded，同 K8s 租约被接管的极性。
    std::fs::rename(&path, root.path().join("retained")).expect("rename");
    std::fs::write(&path, "new-physical-file").expect("replacement");
    assert!(file_receipt_holder_dead(&path, &receipt).expect("superseded inode probe"));
    // 锁文件缺席：同 K8s Lease 对象不存在的极性。
    std::fs::remove_file(&path).expect("remove");
    assert!(file_receipt_holder_dead(&path, &receipt).expect("absent lock probe"));
}

#[cfg(unix)]
#[test]
fn file_receipt_holder_dead_treats_replaced_path_as_superseded() {
    let root = tempfile::tempdir().expect("fixture");
    let path = root.path().join("builder-app.lock");
    let retained = root.path().join("retained");
    let lease = lock_builder_file_with_marker(
        root.path(),
        "builder-app.lock",
        shared_types::AppFileMutationMarker::for_operation("original").expect("marker"),
    )
    .expect("lease");
    let receipt = lease.receipt().expect("receipt");
    drop(lease);
    std::fs::rename(&path, &retained).expect("rename");
    std::os::unix::fs::symlink(&retained, &path).expect("alias");
    assert!(file_receipt_holder_dead(&path, &receipt).expect("replaced path is superseded"));
}

/// D2 同族（Docker 面）：锁文件已缺失 = 确定的已释放态——release 必须
/// Ok（无可完成/解锁的对象）、validate 必须 Ok(false)（无 authority），
/// 否则终态租约清扫对缺失文件的绑定永久重试。
#[cfg(unix)]
#[test]
fn absent_lock_file_is_a_released_state() {
    let root = tempfile::tempdir().expect("fixture");
    let path = root.path().join("builder-app.lock");
    let lease = lock_builder_file_with_marker(
        root.path(),
        "builder-app.lock",
        shared_types::AppFileMutationMarker::for_operation("gone").expect("marker"),
    )
    .expect("lease");
    let receipt = lease.receipt().expect("receipt");
    drop(lease);
    std::fs::remove_file(&path).expect("remove lock file");
    assert_eq!(
        release_file_receipt(&path, &receipt).ok(),
        Some(()),
        "缺失锁文件 = 已释放，不得当成清理错误"
    );
    assert_eq!(
        validate_file_receipt(&path, &receipt).ok(),
        Some(false),
        "缺失锁文件无 authority"
    );
}

#[tokio::test]
async fn durable_operation_marker_echoes_identity_without_authorizing_takeover() {
    let root = tempfile::tempdir().unwrap();
    let name = "builder-durable.lock";
    let marker = shared_types::AppFileMutationMarker::for_operation("operation-one").unwrap();
    let lease = lock_builder_file_with_marker(root.path(), name, marker).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let physical = std::fs::metadata(root.path().join(name)).unwrap();
        let receipt = lease.receipt().expect("durable lease receipt");
        assert_eq!(
            receipt,
            shared_types::UserAppOperationLeaseReceipt::Docker {
                service_type: ServiceType::UserappBuilder,
                device: physical.dev(),
                inode: physical.ino(),
                token: "operation-one".into(),
            }
        );
        receipt.validate().expect("valid physical receipt");
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join(name)).unwrap(),
        "operation-one"
    );
    Box::new(lease).release().await.unwrap();
    assert!(std::fs::read(root.path().join(name)).unwrap().is_empty());

    let marker = shared_types::AppFileMutationMarker::for_operation("operation-one").unwrap();
    drop(lock_builder_file_with_marker(root.path(), name, marker).unwrap());
    // Even an identical durable operation must prove recovery safety first.
    let marker = shared_types::AppFileMutationMarker::for_operation("operation-one").unwrap();
    assert!(lock_builder_file_with_marker(root.path(), name, marker).is_err());
    assert_eq!(
        std::fs::read_to_string(root.path().join(name)).unwrap(),
        "operation-one"
    );
}

#[tokio::test]
async fn failed_builder_completion_clears_only_confirmed_rejection_marker() {
    for status in [403, 409, 408, 500] {
        let root = tempfile::tempdir().unwrap();
        let lease = lock_builder_file(root.path(), "builder-test.lock").unwrap();
        let failure = super::super::builder_completion::docker_error(
            crate::DockerError::BollardError(bollard::errors::Error::DockerResponseServerError {
                status_code: status,
                message: "failed".into(),
            }),
        );
        let result: container_runtime_api::ContainerRuntimeResult<()> =
            super::super::builder_completion::finish(Box::new(lease), Err(failure)).await;
        assert!(result.is_err());
        let next = lock_builder_file(root.path(), "builder-test.lock");
        if matches!(status, 403 | 409) {
            Box::new(next.expect("confirmed rejection permits retry"))
                .release()
                .await
                .unwrap();
        } else {
            assert!(next.is_err(), "unconfirmed outcome must require recovery");
        }
    }
}

#[tokio::test]
async fn failed_marker_release_does_not_erase_other_owner_or_original_error() {
    let root = tempfile::tempdir().unwrap();
    let lease = lock_builder_file(root.path(), "builder-test.lock").unwrap();
    std::fs::write(root.path().join("builder-test.lock"), "replacement-owner").unwrap();
    let failure = container_runtime_api::ContainerRuntimeError::RequestRejected(
        shared_types::RuntimeRequestRejection::from_status(403, "original denial".into()).unwrap(),
    );
    let result: container_runtime_api::ContainerRuntimeResult<()> =
        super::super::builder_completion::finish(Box::new(lease), Err(failure)).await;
    assert!(result.unwrap_err().to_string().contains("original denial"));
    assert_eq!(
        std::fs::read_to_string(root.path().join("builder-test.lock")).unwrap(),
        "replacement-owner"
    );
    assert!(lock_builder_file(root.path(), "builder-test.lock").is_err());
}
#[tokio::test]
async fn captured_delete_retires_only_deleted_identity_caches() {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let name = "rcoder-app-builder-one";
    let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
    tokio::spawn(actor.run());
    let manager = Arc::new(crate::DockerManager {
        docker: bollard::Docker::connect_with_http(
            &format!("http://{address}"),
            1,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap(),
        config: crate::DockerManagerConfig::default(),
        containers,
        main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
        api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
    });
    let query = |id: &str| {
        Arc::new(crate::ContainerQueryResult::new(
            id.into(),
            name.into(),
            crate::ContainerStatus::Running,
            true,
            "192.0.2.1".into(),
            chrono::Utc::now(),
        ))
    };
    manager
        .api_cache
        .insert_status("old-id".into(), Some(query("old-id")))
        .await;
    manager
        .api_cache
        .insert_status(name.into(), Some(query("old-id")))
        .await;
    manager
        .api_cache
        .insert_network("old-id".into(), Some(Arc::new(Default::default())))
        .await;
    manager
        .containers
        .insert(
            "old-alias".into(),
            crate::DockerContainerInfo::new(
                "old-id".into(),
                name.into(),
                "one".into(),
                "image".into(),
            ),
        )
        .await;
    manager
        .containers
        .insert(
            "new-alias".into(),
            crate::DockerContainerInfo::new(
                "new-id".into(),
                name.into(),
                "one".into(),
                "image".into(),
            ),
        )
        .await;
    let writer = manager.clone();
    let replacement = query("new-id");
    let server = tokio::spawn(async move {
        for phase in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 8192];
            let count = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            let (status,body) = match phase {
                    0 => (200, serde_json::json!({"Id":"old-id","Name":format!("/{name}"),"Config":{"Labels":{"service-type":ServiceType::UserappBuilder.to_string(),"identifier":"one"}}}).to_string()),
                    1 => {
                        assert!(request.starts_with("DELETE ") && request.contains("/containers/old-id?"),"{request}");
                        writer.api_cache.insert_status(name.into(),Some(replacement.clone())).await;
                        (204,String::new())
                    },
                    _ => (404,r#"{"message":"not found"}"#.into()),
                };
            let response = format!(
                "HTTP/1.1 {status} response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let runtime = DockerRuntime::new(manager.clone());
    let snapshot = BuilderDeletionSnapshot {
        resource_binding: None,
        app_id: "one".into(),
        operation_id: "test".into(),
        docker_bind_cleanup: true,
        resources: vec![AppResourceIdentity {
            kind: AppResourceKind::Container,
            name: name.into(),
            uid: "old-id".into(),
            resource_version: None,
        }],
    };
    runtime.delete_captured_builder(&snapshot).await.unwrap();
    server.await.unwrap();
    assert!(manager.containers.get("old-alias").await.is_none());
    assert_eq!(
        manager
            .containers
            .get("new-alias")
            .await
            .unwrap()
            .container_id,
        "new-id"
    );
    assert!(manager.api_cache.get_status("old-id").await.is_none());
    assert!(manager.api_cache.get_network("old-id").await.is_none());
    assert_eq!(
        manager
            .api_cache
            .get_status(name)
            .await
            .unwrap()
            .unwrap()
            .container_id,
        "new-id"
    );
}

#[test]
fn captured_builder_requires_actual_family_and_identifier_labels() {
    let inspect = |family: &str, identifier: &str| {
        serde_json::from_value(serde_json::json!({
            "Id": "immutable-docker-id", "Name": "/rcoder-app-builder-one",
            "Config": { "Labels": {"service-type": family, "identifier": identifier} }
        }))
        .unwrap()
    };
    let name = "rcoder-app-builder-one";
    let family = ServiceType::UserappBuilder.to_string();
    let valid = builder_identity(inspect(&family, "one"), name, "one").unwrap();
    assert_eq!(valid.uid, "immutable-docker-id");
    assert_eq!(valid.name, name);
    for foreign in [
        ServiceType::Userapp,
        ServiceType::WebAgentRunner,
        ServiceType::ComputerAgentRunner,
    ] {
        assert!(matches!(
            builder_identity(inspect(&foreign.to_string(), "one"), name, "one"),
            Err(Error::Conflict(_))
        ));
    }
    assert!(matches!(
        builder_identity(inspect(&family, "other"), name, "one"),
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        builder_identity(
            bollard::models::ContainerInspectResponse::default(),
            name,
            "one"
        ),
        Err(Error::Conflict(_))
    ));
    let mut missing_id = inspect(&family, "one");
    missing_id.id = Some(String::new());
    assert!(matches!(
        builder_identity(missing_id, name, "one"),
        Err(Error::DockerError(_))
    ));
}

#[tokio::test]
async fn completed_builder_file_lease_allows_recreation() {
    let root = tempfile::tempdir().unwrap();
    let captured = lock_builder_file(root.path(), "builder-one.lock").unwrap();
    assert!(matches!(
        lock_builder_file(root.path(), "builder-one.lock"),
        Err(Error::Conflict(_))
    ));
    let unrelated = lock_builder_file(root.path(), "builder-two.lock").unwrap();
    Box::new(unrelated).release().await.unwrap();
    Box::new(captured).release().await.unwrap();
    let next = lock_builder_file(root.path(), "builder-one.lock").unwrap();
    Box::new(next).release().await.unwrap();
}
#[test]
fn undelivered_lock_acquisition_does_not_strand_marker() {
    let root = tempfile::tempdir().unwrap();
    drop(UnclaimedBuilderLease(Some(
        lock_builder_file(root.path(), "builder-one.lock").unwrap(),
    )));
    let reclaimed = lock_builder_file(root.path(), "builder-one.lock").unwrap();
    drop(UnclaimedBuilderLease(Some(reclaimed)));
}
#[test]
fn uncertain_builder_mutation_retains_marker_after_file_unlock() {
    let root = tempfile::tempdir().unwrap();
    drop(lock_builder_file(root.path(), "builder-one.lock").unwrap());
    assert!(matches!(
        lock_builder_file(root.path(), "builder-one.lock"),
        Err(Error::Conflict(_))
    ));
}
#[tokio::test]
async fn builder_drop_unlocks_live_duplicate_and_preserves_only_uncertain_marker() {
    for completed in [false, true] {
        let root = tempfile::tempdir().expect("directory");
        let lease = lock_builder_file(root.path(), "builder-duplicate.lock").expect("lease");
        let duplicate = lease.file.try_clone().expect("duplicate descriptor");
        if completed {
            Box::new(lease).release().await.expect("complete");
        } else {
            drop(lease);
        }
        let next = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.path().join("builder-duplicate.lock"))
            .expect("next file");
        next.try_lock()
            .expect("Drop explicitly unlocked despite live duplicate");
        assert_eq!(next.metadata().expect("metadata").len() > 0, !completed);
        next.unlock().expect("next unlock");
        drop(duplicate);
    }
}
