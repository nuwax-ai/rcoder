use super::tests::receipt_env;
use super::*;

async fn resume_case(initially_running: bool, management_boot: bool, management_request: bool) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let receipt_root = tempfile::tempdir().expect("receipt root");
    receipt_env::set_root(receipt_root.path());
    let context = UserAppExecutionContext {
        app_id: "app".into(),
        lifecycle_id: "life".into(),
        operation_id: "source-build".into(),
        executor_id: "worker".into(),
        request_fingerprint: "b".repeat(64),
    };
    let mut labels = context.resource_metadata();
    labels.insert("rcoder.io/request-fingerprint".into(), "a".repeat(64));
    labels.insert(
        "service-type".into(),
        ServiceType::UserappBuilder.to_string(),
    );
    labels.insert("identifier".into(), "app".into());
    let mut env = vec!["USER_ID=owner".to_string()];
    if management_boot {
        env.push(format!("{}=true", shared_types::APP_CLI_CONTROL_ONLY_ENV));
        env.push(format!(
            "{}=source-build",
            shared_types::APP_CLI_MANAGEMENT_BOOT_ID_ENV
        ));
    }
    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(initially_running));
    let server_running = running.clone();
    let mut object = serde_json::json!({"Id":"original-id","Created":"2026-01-01T00:00:00Z",
        "Config":{"Image":"previous-runtime:1","Labels":labels,"Env":env},
        "State":{"Running":true},"HostConfig":{"NetworkMode":"test"},
        "NetworkSettings":{"Networks":{"test":{"IPAddress":"172.20.0.4"}}}});
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let address = listener.local_addr().expect("address");
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recorded = requests.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let n = stream.read(&mut buffer).await.expect("read");
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..n]);
                if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            if bytes.is_empty() {
                continue;
            }
            let headers = String::from_utf8_lossy(&bytes);
            let first = headers.lines().next().expect("request");
            recorded.lock().expect("requests").push(first.into());
            assert!(
                first.starts_with("GET ") && first.contains("/json")
                    || first.starts_with("POST ")
                        && first.contains("/containers/original-id/start"),
                "existing compute must not be pulled, created, removed or rebuilt: {first}"
            );
            let (code, body) = if first.starts_with("POST ") {
                server_running.store(true, std::sync::atomic::Ordering::SeqCst);
                (204, String::new())
            } else {
                object["State"]["Running"] = server_running
                    .load(std::sync::atomic::Ordering::SeqCst)
                    .into();
                (200, object.to_string())
            };
            stream.write_all(format!("HTTP/1.1 {code} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
                .await.expect("response");
        }
    });
    let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
    let actor_task = tokio::spawn(actor.run());
    let manager = std::sync::Arc::new(crate::DockerManager {
        docker: bollard::Docker::connect_with_http(
            &format!("http://{address}"),
            1,
            bollard::API_DEFAULT_VERSION,
        )
        .expect("Docker client"),
        config: crate::DockerManagerConfig::default(),
        containers,
        main_network_name: std::sync::Arc::new(tokio::sync::RwLock::new("test".into())),
        api_cache: std::sync::Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
    });
    let runtime = DockerRuntime::new(manager);
    let mut params = container_runtime_api::ContainerCreateParams::builder()
        .project_id("app".to_string())
        .service_type(ServiceType::UserappBuilder)
        .execution_context(context)
        .build();
    params.resource_binding = Some(shared_types::UserAppResourceBinding {
        app_id: "app".into(),
        lifecycle_id: "life".into(),
        service_type: ServiceType::UserappBuilder,
        physical_uid: "original-id".into(),
        adopted_by_operation: "adopt".into(),
    });
    if management_request {
        params.builder_startup_mode = shared_types::BuilderStartupMode::ManagementOnly;
    }
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        runtime.resume_bound_builder(&params, "new-runtime:2".into()),
    )
    .await
    .expect("resume budget");
    if management_request && !initially_running && !management_boot {
        assert!(
            matches!(result, Err(Error::ConfigurationError(_))),
            "old exited env must return to the controlled restore: {result:?}"
        );
        assert!(
            requests
                .lock()
                .expect("requests")
                .iter()
                .all(|request| request.starts_with("GET "))
        );
    } else {
        assert_eq!(
            result.expect("owned release is usable").container_id,
            "original-id"
        );
    }
    assert!(!requests.lock().expect("requests").is_empty());
    server.abort();
    actor_task.abort();
}

#[tokio::test]
async fn bound_resume_keeps_previous_image_and_creation_digest_usable() {
    resume_case(true, false, false).await;
}
#[tokio::test]
async fn management_only_does_not_start_exited_legacy_environment() {
    resume_case(false, false, true).await;
}
#[tokio::test]
async fn management_only_starts_after_restored_environment_and_keeps_running_business_untouched() {
    resume_case(false, true, true).await;
    resume_case(true, false, true).await;
}
