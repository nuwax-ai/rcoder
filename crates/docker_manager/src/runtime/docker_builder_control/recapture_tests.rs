//! Preserve a damaged archive while recapturing an actually stopped builder.
use super::tests::receipt_env;
use super::*;
use container_runtime_api::AgentContainerRuntime as _;
use sha2::Digest as _;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn damaged_original_archive_recaptures_original_stopped_configuration_without_compute_writes()
{
    let receipts = tempfile::tempdir().expect("private receipts");
    let workspace = tempfile::tempdir().expect("retained workspace");
    std::fs::write(workspace.path().join("sentinel"), b"retained-data").expect("sentinel");
    receipt_env::set_root(receipts.path());
    let context = UserAppExecutionContext {
        app_id: "app".into(),
        lifecycle_id: "life".into(),
        operation_id: "stopped-original-request".into(),
        executor_id: "worker".into(),
        request_fingerprint: "a".repeat(64),
    };
    let mut labels = context.resource_metadata();
    labels.insert(
        "service-type".into(),
        ServiceType::UserappBuilder.to_string(),
    );
    labels.insert("identifier".into(), "app".into());
    let object = serde_json::json!({"Id":"original-id","Config":{"Image":"old-runtime:1","Labels":labels,"Env":["APP_CLI_DEPLOY_TOKEN=original-private-token","APPLICATION_SETTING=original-value"]},
        "HostConfig":{"AutoRemove":false,"NetworkMode":"test","Binds":[format!("{}:/home/user/app",workspace.path().display())]},
        "Mounts":[{"Type":"bind","Source":workspace.path(),"Destination":"/home/user/app","RW":true}],
        "NetworkSettings":{"Networks":{"test":{"IPAddress":"172.20.0.4"}}},
        "State":{"Running":true,"Status":"running","Restarting":false,"Dead":false}});
    let state = Arc::new(Mutex::new(object));
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let address = listener.local_addr().expect("address");
    let served = state.clone();
    let recorded = requests.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let state = served.clone();
            let requests = recorded.clone();
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    let count = stream.read(&mut buffer).await.expect("request read");
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                    if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                        break;
                    }
                }
                let headers = String::from_utf8_lossy(&bytes);
                let first = headers.lines().next().expect("request").to_owned();
                requests.lock().expect("requests").push(first.clone());
                let (status, body) = {
                    let mut object = state.lock().expect("state");
                    if first.starts_with("POST ") && first.contains("/containers/original-id/stop")
                    {
                        object["State"]["Running"] = false.into();
                        object["State"]["Status"] = "exited".into();
                        (204, String::new())
                    } else if object.is_null() {
                        (
                            404,
                            serde_json::json!({"message":"fixture container absent"}).to_string(),
                        )
                    } else {
                        assert!(
                            first.starts_with("GET ")
                                && first.contains("/containers/")
                                && first.contains("/json"),
                            "must not recreate, remove, start, pull or mutate configuration: {first}"
                        );
                        (200, object.to_string())
                    }
                };
                stream.write_all(format!("HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.expect("reply");
            });
        }
    });
    let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
    let actor_task = tokio::spawn(actor.run());
    let manager = Arc::new(crate::DockerManager {
        docker: bollard::Docker::connect_with_http(
            &format!("http://{address}"),
            1,
            bollard::API_DEFAULT_VERSION,
        )
        .expect("Docker client"),
        config: crate::DockerManagerConfig::default(),
        containers,
        main_network_name: Arc::new(tokio::sync::RwLock::new("test".into())),
        api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
    });
    let runtime = DockerRuntime::new(manager);
    let binding = shared_types::UserAppResourceBinding {
        app_id: "app".into(),
        lifecycle_id: "life".into(),
        service_type: ServiceType::UserappBuilder,
        physical_uid: "original-id".into(),
        adopted_by_operation: "adopt".into(),
    };
    let mut source = runtime
        .capture_builder_compute_with_binding(&context, Some(&binding), false)
        .await
        .expect("original capture");
    source.restart_image = Some("frozen-paired-runtime:2".into());
    source.builder_startup_mode = shared_types::BuilderStartupMode::ManagementOnly;
    let original = runtime
        .archive_builder_restart(&source)
        .await
        .expect("original archive")
        .expect("archive");
    let before_inspection = requests.lock().expect("requests").len();
    assert_eq!(
        runtime
            .inspect_builder_restart_archive(&original)
            .await
            .expect("valid private bytes"),
        Some(true)
    );
    let mut mismatched = original.clone();
    mismatched.source.context.operation_id = "different-operation".into();
    assert_eq!(
        runtime
            .inspect_builder_restart_archive(&mismatched)
            .await
            .expect("source mismatch"),
        Some(false)
    );
    let mut inaccessible = original.clone();
    inaccessible.source.context.app_id = "io-error-app".into();
    inaccessible
        .source
        .resource_binding
        .as_mut()
        .expect("binding")
        .app_id = "io-error-app".into();
    std::fs::write(
        receipts.path().join(".app-operation-receipts/io-error-app"),
        b"parent is a file",
    )
    .expect("I/O failure fixture");
    assert!(
        matches!(
            runtime.inspect_builder_restart_archive(&inaccessible).await,
            Err(Error::DockerError(_))
        ),
        "a real private-file I/O failure must not look like damaged/missing history"
    );
    assert_eq!(
        requests.lock().expect("requests").len(),
        before_inspection,
        "private archive inspection must not query or write compute"
    );
    assert!(
        runtime
            .recapture_stopped_builder_restart(&source)
            .await
            .is_err(),
        "a running target with no acknowledged original stop cannot recapture"
    );
    runtime
        .apply_builder_control(&source, false)
        .await
        .expect("actual acknowledged stop");
    let old_path = receipts
        .path()
        .join(".app-operation-receipts/app")
        .join(format!("{}.json", original.archive.name));
    let retained_old_path = old_path.with_extension("retained");
    std::fs::rename(&old_path, &retained_old_path).expect("temporarily unavailable history");
    assert_eq!(
        runtime
            .inspect_builder_restart_archive(&original)
            .await
            .expect("missing private file"),
        Some(false)
    );
    std::fs::rename(&retained_old_path, &old_path).expect("preserve original history");
    let damaged = b"{damaged original archive bytes retained}";
    std::fs::write(&old_path, damaged).expect("inject historical damage");
    assert_eq!(
        runtime
            .inspect_builder_restart_archive(&original)
            .await
            .expect("bad hash"),
        Some(false)
    );
    let mut undecodable = original.clone();
    undecodable.archive.uid = sha2::Sha256::digest(damaged)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        runtime
            .inspect_builder_restart_archive(&undecodable)
            .await
            .expect("matching digest, invalid JSON"),
        Some(false)
    );
    let ordinary = runtime.archive_builder_restart(&source).await;
    assert!(
        ordinary.is_err(),
        "ordinary exclusive publication must refuse the occupied content path"
    );
    assert_eq!(std::fs::read(&old_path).expect("old bytes"), damaged);
    let stopped = state.lock().expect("state").clone();
    let saved_files = std::fs::read_dir(receipts.path().join(".app-operation-receipts/app"))
        .expect("private directory")
        .count();
    for case in [
        "running",
        "restarting",
        "dead",
        "unknown",
        "replacement",
        "missing",
    ] {
        let mut changed = stopped.clone();
        match case {
            "running" => {
                changed["State"]["Running"] = true.into();
                changed["State"]["Status"] = "running".into();
            }
            "restarting" => changed["State"]["Restarting"] = true.into(),
            "dead" => changed["State"]["Dead"] = true.into(),
            "unknown" => changed["State"]["Status"] = serde_json::Value::Null,
            "replacement" => changed["Id"] = "foreign-container".into(),
            "missing" => changed = serde_json::Value::Null,
            _ => unreachable!("enumerated case"),
        }
        *state.lock().expect("state") = changed;
        assert!(
            runtime
                .recapture_stopped_builder_restart(&source)
                .await
                .is_err(),
            "{case} cannot produce a fresh template"
        );
        assert_eq!(
            std::fs::read_dir(receipts.path().join(".app-operation-receipts/app"))
                .expect("directory unchanged")
                .count(),
            saved_files,
            "no new private template on rejected {case}"
        );
    }
    *state.lock().expect("state") = stopped;
    let mut another_operation = source.clone();
    another_operation.context.operation_id = "unacknowledged-operation".into();
    assert!(
        runtime
            .recapture_stopped_builder_restart(&another_operation)
            .await
            .is_err(),
        "another operation cannot borrow the original Stop receipt"
    );
    let before = requests.lock().expect("requests").len();
    let recovered = runtime
        .recapture_stopped_builder_restart(&source)
        .await
        .expect("correctly stopped original should recapture without historical admission gate");
    assert_eq!(recovered.source, source);
    assert_eq!(recovered.volumes, original.volumes);
    assert_ne!(
        recovered.archive.name, original.archive.name,
        "a damaged path must never be overwritten"
    );
    assert_eq!(
        recovered.archive.uid, original.archive.uid,
        "same captured config/intent keeps its actual content identity"
    );
    assert_eq!(
        std::fs::read(&old_path).expect("old bytes after recapture"),
        damaged
    );
    let new_path = receipts
        .path()
        .join(".app-operation-receipts/app")
        .join(format!("{}.json", recovered.archive.name));
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&new_path).expect("new private payload"))
            .expect("payload");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&new_path)
                .expect("private archive metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "retained runtime credentials require owner-only bytes"
        );
    }
    assert_eq!(
        runtime
            .inspect_builder_restart_archive(&recovered)
            .await
            .expect("verified recovery bytes"),
        Some(true)
    );
    assert_eq!(saved["source"]["restart_image"], "frozen-paired-runtime:2");
    assert_eq!(saved["source"]["builder_startup_mode"], "management_only");
    assert_eq!(
        saved["env"],
        serde_json::json!([
            "APP_CLI_DEPLOY_TOKEN=original-private-token",
            "APPLICATION_SETTING=original-value"
        ])
    );
    assert_eq!(
        std::fs::read(workspace.path().join("sentinel")).expect("sentinel unchanged"),
        b"retained-data"
    );
    assert!(
        requests.lock().expect("requests")[before..]
            .iter()
            .all(|request| request.starts_with("GET "))
    );
    server.abort();
    actor_task.abort();
}
