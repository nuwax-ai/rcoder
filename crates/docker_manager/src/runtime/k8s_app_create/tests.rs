use super::*;

#[cfg(test)]
mod cases {
    use super::*;

    #[test]
    fn app_flat_volume_mounts_mirror_dev_layout() {
        let mounts = app_flat_volume_mounts("a1");
        assert_eq!(
            mounts,
            [
                ("a1".to_string(), "/home/user/a1".to_string()),
                ("data".to_string(), "/home/user/data".to_string()),
                ("logs".to_string(), "/home/user/logs".to_string()),
                (
                    "agent-store".to_string(),
                    "/home/user/.agent-store".to_string()
                ),
            ]
        );
        // 段序与布局事实源配对：卷内 {app_id}/ 对应宿主树 workspace 段，
        // data/logs/agent-store 平级子目录对应宿主树三数据段的 app 子层
        let subs = shared_types::paths::userapp_prod_subpaths("a1");
        assert_eq!(subs[0], "prod/userapp/a1");
        assert!(mounts[0].0 == "a1" && mounts[0].1.ends_with("/a1"));
        for (m, sub_suffix) in mounts
            .iter()
            .skip(1)
            .zip(["data/a1", "logs/a1", "agent-store/a1"])
        {
            assert!(
                sub_suffix.starts_with(&m.0),
                "卷内平级目录 {m:?} 应是宿主段 {sub_suffix} 的前缀"
            );
        }
    }

    /// 换代配置回收过滤：活跃引用与其它来源命名不回收，同 app 旧代/失败 staging
    /// 残留回收，前缀含结尾 `-` 隔离相近 app id（10 vs 104）。
    #[test]
    fn superseded_generation_filter_keeps_active_and_ignores_foreign_names() {
        // 活跃引用自身不回收（cm 与 sec 各自的活跃名）
        assert!(!is_superseded_generation(
            "104",
            "ua-104-active0123abcd-env",
            "ua-104-active0123abcd-env"
        ));
        assert!(!is_superseded_generation(
            "104",
            "ua-104-active0123abcd-sec",
            "ua-104-active0123abcd-sec"
        ));
        // 同 app 被取代的旧代与失败 staging 残留回收
        assert!(is_superseded_generation(
            "104",
            "ua-104-active0123abcd-env",
            "ua-104-old0456ffff-env"
        ));
        assert!(is_superseded_generation(
            "104",
            "ua-104-active0123abcd-sec",
            "ua-104-stale0789eeee-sec"
        ));
        // 非 ua- 前缀的其它来源配置（apply 路径固定名）不回收
        assert!(!is_superseded_generation(
            "104",
            "ua-104-active0123abcd-env",
            "rcoder-app-104-config"
        ));
        // 前缀隔离：app 104 的对象不被 app 10 的回收误删（含结尾 `-`）
        assert!(!is_superseded_generation(
            "10",
            "ua-10-active0123abcd-env",
            "ua-104-old0456ffff-env"
        ));
    }
}

#[cfg(test)]
mod conditional_tests {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn runtime(client: kube::Client) -> KubernetesRuntime {
        use crate::runtime::kubernetes_runtime::KubernetesRuntimeConfig;
        KubernetesRuntime {
            client,
            namespace: "review-test".into(),
            config: KubernetesRuntimeConfig {
                namespace: "review-test".into(),
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
            event_counters: Arc::new(
                crate::runtime::k8s_event_publisher::PublisherCounters::default(),
            ),
        }
    }

    #[tokio::test]
    async fn owned_pvc_create_echoes_identity_and_validates_competing_winner() {
        use crate::runtime::k8s_pvc::K8sPvcOps as _;
        tokio::time::timeout(std::time::Duration::from_secs(12), async {
            for outcome in ["created", "same-lifecycle", "foreign-lifecycle", "wrong-access"] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
                let address = listener.local_addr().expect("address");
                drop(rustls::crypto::ring::default_provider().install_default());
                let config = kube::Config::new(format!("http://{address}").parse().expect("URI"));
                let runtime = runtime(kube::Client::try_from(config).expect("client"));
                let name = runtime.workspace_pvc_name("fence", &ServiceType::Userapp).expect("PVC name");
                let server = tokio::spawn(async move {
                    let mut winner = serde_json::Value::Null;
                    for step in 0..if outcome == "created" {2} else {3} {
                        let (mut stream, _) = listener.accept().await.expect("accept");
                        let mut bytes = Vec::new(); let mut chunk = [0u8; 4096];
                        let (head, offset, length) = loop {
                            let count = stream.read(&mut chunk).await.expect("read"); assert!(count > 0);
                            bytes.extend_from_slice(&chunk[..count]);
                            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
                                let length = head.lines().find_map(|line| {
                                    let (key, value) = line.split_once(':')?;
                                    key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().expect("length"))
                                }).unwrap_or(0);
                                break (head, end + 4, length);
                            }
                        };
                        while bytes.len() < offset + length {
                            let count = stream.read(&mut chunk).await.expect("body"); assert!(count > 0); bytes.extend_from_slice(&chunk[..count]);
                        }
                        let (status, reply) = match step {
                            0 => {
                                assert!(head.starts_with("GET ") && head.contains(&name), "{head}");
                                (404, serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","message":"not created","code":404}))
                            }
                            1 => {
                                assert!(head.starts_with("POST ") && head.contains("/persistentvolumeclaims"), "{head}");
                                winner = serde_json::from_slice(&bytes[offset..offset+length]).expect("PVC POST");
                                assert_eq!(winner["metadata"]["name"], name);
                                assert_eq!(winner["metadata"]["annotations"]["rcoder.io/application-id"], "fence");
                                assert_eq!(winner["metadata"]["annotations"]["rcoder.io/lifecycle-id"], "life-one");
                                assert_eq!(winner["metadata"]["annotations"]["rcoder.io/storage-use-operation"], "create-one");
                                assert_eq!(winner["spec"]["resources"]["requests"]["storage"], "7Gi");
                                winner["metadata"]["uid"] = "winner-uid".into(); winner["metadata"]["resourceVersion"] = "8".into();
                                if outcome == "created" { (201, winner.clone()) }
                                else { (409, serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"AlreadyExists","message":"winner committed","code":409})) }
                            }
                            2 => {
                                assert!(head.starts_with("GET ") && head.contains(&name), "creation conflict must inspect winner: {head}");
                                if outcome == "foreign-lifecycle" { winner["metadata"]["annotations"]["rcoder.io/lifecycle-id"] = "replacement-life".into(); }
                                if outcome == "wrong-access" { winner["spec"]["accessModes"] = serde_json::json!(["ReadWriteMany"]); }
                                (200, winner.clone())
                            }
                            _ => unreachable!(),
                        };
                        let body = serde_json::to_vec(&reply).expect("reply");
                        stream.write_all(format!("HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.expect("headers");
                        stream.write_all(&body).await.expect("body");
                    }
                });
                let context = shared_types::UserAppExecutionContext {
                    app_id:"fence".into(),lifecycle_id:"life-one".into(),operation_id:"create-one".into(),executor_id:"executor-one".into(),request_fingerprint:"a".repeat(64),
                };
                let result = runtime.ensure_owned_workspace_pvc(&context, &ServiceType::Userapp, Some("7Gi")).await;
                if matches!(outcome, "created" | "same-lifecycle") { result.expect("owned PVC established"); }
                else { assert!(matches!(result, Err(ContainerRuntimeError::Conflict(_))), "{outcome}: {result:?}"); }
                server.await.expect("PVC wire assertions");
            }
        }).await.expect("bounded owned PVC creation scenarios");
    }

    #[tokio::test]
    async fn pvc_expansion_sends_captured_identity_and_propagates_conflict() {
        use crate::runtime::k8s_pvc::K8sPvcOps as _;
        tokio::time::timeout(std::time::Duration::from_secs(8), async {
            for conflict in [false, true] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
                let address = listener.local_addr().expect("address");
                drop(rustls::crypto::ring::default_provider().install_default());
                let config = kube::Config::new(format!("http://{address}").parse().expect("URI"));
                let runtime = runtime(kube::Client::try_from(config).expect("client"));
                let name = runtime.workspace_pvc_name("fence", &ServiceType::Userapp).expect("name");
                let server = tokio::spawn(async move {
                    let pvc = serde_json::json!({"apiVersion":"v1","kind":"PersistentVolumeClaim",
                        "metadata":{"name":name,"uid":"original-storage","resourceVersion":"47","labels":{"service_type":ServiceType::Userapp.to_string()}},
                        "spec":{"resources":{"requests":{"storage":"1Gi"}}}});
                    for step in 0..2 {
                        let (mut stream, _) = listener.accept().await.expect("accept");
                        let mut bytes = Vec::new(); let mut chunk = [0u8; 4096];
                        let (head, offset, length) = loop {
                            let count = stream.read(&mut chunk).await.expect("read"); assert!(count > 0);
                            bytes.extend_from_slice(&chunk[..count]);
                            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
                                let length = head.lines().find_map(|line| {
                                    let (key, value) = line.split_once(':')?;
                                    key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().expect("length"))
                                }).unwrap_or(0);
                                break (head, end + 4, length);
                            }
                        };
                        while bytes.len() < offset + length {
                            let count = stream.read(&mut chunk).await.expect("body"); assert!(count > 0); bytes.extend_from_slice(&chunk[..count]);
                        }
                        assert!(head.contains(&format!("/persistentvolumeclaims/{name}")), "{head}");
                        if step == 0 { assert!(head.starts_with("GET "), "{head}"); }
                        else {
                            assert!(head.starts_with("PATCH "), "{head}");
                            let patch: serde_json::Value = serde_json::from_slice(&bytes[offset..offset+length]).expect("patch");
                            assert_eq!(patch["metadata"]["uid"], "original-storage");
                            assert_eq!(patch["metadata"]["resourceVersion"], "47");
                            assert_eq!(patch["spec"]["resources"]["requests"]["storage"], "2Gi");
                        }
                        let rejected = step == 1 && conflict;
                        let status = if rejected {409} else {200};
                        let body = serde_json::to_vec(&if rejected {
                            serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Conflict","message":"replacement PVC","code":409})
                        } else {pvc.clone()}).expect("reply");
                        stream.write_all(format!("HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.expect("headers");
                        stream.write_all(&body).await.expect("reply");
                    }
                });
                let result = runtime.resize_app_pvc("fence", "2Gi").await;
                if conflict { assert!(matches!(result, Err(ContainerRuntimeError::Conflict(_))), "{result:?}"); }
                else { assert!(matches!(result, Ok(container_runtime_api::StorageResizeOutcome::Resized {from, to}) if from == "1Gi" && to == "2Gi")); }
                server.await.expect("wire assertions");
            }
        }).await.expect("bounded PVC expansion scenario");
    }

    #[tokio::test]
    async fn update_rejects_replacement_lifecycle_before_pvc_or_config_requests() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let address = listener.local_addr().expect("address");
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut request = Vec::new();
                let mut chunk = [0u8; 1024];
                while !request.windows(4).any(|value| value == b"\r\n\r\n") {
                    let count = stream.read(&mut chunk).await.expect("read");
                    assert!(count > 0); request.extend_from_slice(&chunk[..count]);
                }
                let request = String::from_utf8_lossy(&request);
                assert!(request.starts_with("GET ") && request.contains("/deployments/rcoder-app-fence"), "identity check must precede all resource effects: {request}");
                let body = serde_json::to_vec(&serde_json::json!({
                    "apiVersion":"apps/v1","kind":"Deployment",
                    "metadata":{"name":"rcoder-app-fence","uid":"replacement-uid","resourceVersion":"71",
                        "labels":{"app.kubernetes.io/name":"user-app","app.kubernetes.io/instance":"fence","app.kubernetes.io/managed-by":"rcoder-app-manager","app.kubernetes.io/part-of":"rcoder","rcoder.io/app-id":"fence"},
                        "annotations":{"rcoder.io/application-id":"fence","rcoder.io/owner-id":"owner-one","rcoder.io/lifecycle-id":"replacement-life"}}
                })).expect("reply");
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.expect("headers");
                stream.write_all(&body).await.expect("reply");
            });
            drop(rustls::crypto::ring::default_provider().install_default());
            let config = kube::Config::new(format!("http://{address}").parse().expect("URI"));
            let runtime = runtime(kube::Client::try_from(config).expect("client"));
            let params = ContainerCreateParams::builder().project_id("fence").user_id("owner-one").service_type(ServiceType::Userapp).execution_context(shared_types::UserAppExecutionContext {
                app_id:"fence".into(),lifecycle_id:"old-life".into(),operation_id:"update-one".into(),executor_id:"executor-one".into(),request_fingerprint:"a".repeat(64),
            }).build();
            let result = runtime.write_app_resources("fence", &params, None, None, HttpExpose::Pingora, Some("71")).await;
            // 捕获阶段的身份核验拒绝以 CreationAborted 结构化上抛：失败阶段 =
            // Capture（先于一切资源副作用）、definitive_rejection、无遗留幂等
            // 资源；根因 Conflict 指明 lifecycle-id 漂移。
            assert!(
                matches!(
                    &result,
                    Err(ContainerRuntimeError::CreationAborted { progress, source })
                        if progress.failed_at == container_runtime_api::CreationStage::Capture
                            && progress.definitive_rejection
                            && progress.retained_idempotent_resources.is_empty()
                            && matches!(
                                source.as_ref(),
                                ContainerRuntimeError::Conflict(message)
                                    if message.contains("lifecycle-id")
                            )
                ),
                "old lifecycle must be rejected at capture, before any resource effects: {result:?}"
            );
            server.await.expect("request assertions");
        }).await.expect("bounded update identity scenario");
    }

    #[tokio::test]
    async fn workload_mutations_send_preconditions_and_do_not_retry_rejections() {
        // Real HTTP transport and Kubernetes serialization; no cluster resources.
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            for action in ["scale", "wake", "recycle", "stop"] {
                for rejection in [0, 409, 403] {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
                    let address = listener.local_addr().expect("address");
                    let server = tokio::spawn(async move {
                        let deployment = serde_json::json!({
                            "apiVersion":"apps/v1", "kind":"Deployment",
                            "metadata":{"name":"rcoder-app-fence","uid":"captured-original","resourceVersion":"71",
                                "annotations":{"rcoder.io/application-id":"fence","rcoder.io/owner-id":"owner-one","rcoder.io/lifecycle-id":"life-one"},
                                "labels":{"app.kubernetes.io/name":"user-app","app.kubernetes.io/instance":"fence",
                                    "app.kubernetes.io/managed-by":"rcoder-app-manager","app.kubernetes.io/part-of":"rcoder","rcoder.io/app-id":"fence"}},
                            "spec":{"replicas":1,"selector":{"matchLabels":{"app":"fence"}},"template":{"spec":{"containers":[{"name":"app","image":"fixture"}]}}}
                        });
                        for step in 0..2 {
                            let (mut stream, _) = listener.accept().await.expect("accept");
                            let mut bytes = Vec::new();
                            let mut buffer = [0u8; 4096];
                            let (head, offset, length) = loop {
                                let count = stream.read(&mut buffer).await.expect("read");
                                assert!(count > 0, "unexpected request EOF");
                                bytes.extend_from_slice(&buffer[..count]);
                                if let Some(end) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
                                    let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
                                    let length = head.lines().find_map(|line| {
                                        let (key, value) = line.split_once(':')?;
                                        key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().expect("length"))
                                    }).unwrap_or(0);
                                    break (head, end + 4, length);
                                }
                            };
                            while bytes.len() < offset + length {
                                let count = stream.read(&mut buffer).await.expect("body");
                                assert!(count > 0);
                                bytes.extend_from_slice(&buffer[..count]);
                            }
                            assert!(head.contains("/deployments/rcoder-app-fence"), "{head}");
                            let (status, reply) = if step == 0 {
                                assert!(head.starts_with("GET "), "{head}");
                                (200, deployment.clone())
                            } else {
                                assert!(head.starts_with("PATCH "), "{head}");
                                assert!(head.to_ascii_lowercase().contains("application/merge-patch+json"), "{head}");
                                let patch: serde_json::Value = serde_json::from_slice(&bytes[offset..offset + length]).expect("patch");
                                assert_eq!(patch["metadata"]["uid"], "captured-original");
                                assert_eq!(patch["metadata"]["resourceVersion"], "71");
                                match action {
                                    "stop" => {
                                        assert_eq!(patch["spec"]["replicas"], 0);
                                        assert_eq!(patch["metadata"]["annotations"]["rcoder.io/wake-on-traffic"], "false");
                                    }
                                    "scale" => assert_eq!(patch["spec"]["replicas"], 0),
                                    "wake" => assert_eq!(patch["metadata"]["annotations"]["rcoder.io/wake-on-traffic"], "false"),
                                    "recycle" => assert_eq!(patch["metadata"]["annotations"]["rcoder.io/recycle-enabled"], "false"),
                                    _ => unreachable!(),
                                }
                                if rejection == 0 { (200, deployment.clone()) }
                                else { (rejection, serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":if rejection == 409 {"Conflict"} else {"Forbidden"},"message":"write rejected","code":rejection})) }
                            };
                            let reply = serde_json::to_vec(&reply).expect("reply");
                            stream.write_all(format!("HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reply.len()).as_bytes()).await.expect("headers");
                            stream.write_all(&reply).await.expect("response");
                        }
                        // Dropping the listener makes an unintended retry fail,
                        // rather than accepting a later unconditional write.
                    });
                    drop(rustls::crypto::ring::default_provider().install_default());
                    let config = kube::Config::new(format!("http://{address}").parse().expect("URI"));
                    let runtime = runtime(kube::Client::try_from(config).expect("client"));
                    let result = match action {
                        "stop" => {
                            let context = shared_types::UserAppExecutionContext {
                                app_id: "fence".into(), lifecycle_id: "life-one".into(),
                                operation_id: "stop-one".into(), executor_id: "executor-one".into(), request_fingerprint: "a".repeat(64),
                            };
                            let target = runtime.capture_stop_target(&context, Some("71")).await.expect("capture stop target");
                            runtime.stop_captured_target(&target, false).await
                        }
                        "scale" => runtime.scale_app("fence", 0).await,
                        "wake" => runtime.patch_app_wake_on_traffic("fence", false).await,
                        "recycle" => runtime.patch_app_recycle_policy("fence", Some(false), None).await,
                        _ => unreachable!(),
                    };
                    match rejection {
                        0 => result.expect("conditional commit"),
                        409 => assert!(matches!(result, Err(ContainerRuntimeError::Conflict(_))), "{result:?}"),
                        403 => assert!(matches!(result, Err(ContainerRuntimeError::RequestRejected(ref rejection)) if rejection.status == 403 && rejection.message.contains("write rejected")), "typed explicit refusal must remain visible: {result:?}"),
                        _ => unreachable!("fixture only uses success, conflict or forbidden"),
                    }
                    server.await.expect("wire assertions");
                }
            }
        }).await.expect("bounded API contract scenario");
    }

    #[tokio::test]
    async fn hot_env_commit_sends_resource_version_and_preserves_conflict() {
        use container_runtime_api::UserAppDeploymentRuntime;
        for conflict in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let address = listener.local_addr().expect("address");
            let target = std::collections::HashMap::from([
                (
                    "APP_DEPLOY_OPERATION_ID".to_owned(),
                    "operation-b".to_owned(),
                ),
                (
                    "APP_DEPLOY_GENERATION_ID".to_owned(),
                    "generation".to_owned(),
                ),
            ]);
            let desired = target.clone();
            let server = tokio::spawn(async move {
                for step in 0..if conflict { 3 } else { 5 } {
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut bytes = Vec::new();
                    let mut buffer = [0u8; 4096];
                    let (head, offset, length) = loop {
                        let n = stream.read(&mut buffer).await.expect("read");
                        assert!(n > 0);
                        bytes.extend_from_slice(&buffer[..n]);
                        if let Some(offset) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&bytes[..offset]).into_owned();
                            let length = head
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|n| n.trim().parse::<usize>().expect("length"))
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
                    let (status, reply) = match step {
                        0 | 3 => {
                            assert!(
                                head.starts_with("GET ") && head.contains("/deployments/"),
                                "{head}"
                            );
                            (
                                200,
                                serde_json::json!({"apiVersion":"apps/v1","kind":"Deployment",
                                "metadata":{"name":"rcoder-app-hot","uid":"deployment-owned","resourceVersion":"10"},
                                "spec":{"selector":{"matchLabels":{"app":"hot"}},"template":{"spec":{"containers":[
                                    {"name":"app","envFrom":[{"configMapRef":{"name":"hot-env"}}]}
                                ]}}}}),
                            )
                        }
                        1 | 4 => {
                            assert!(
                                head.starts_with("GET ") && head.contains("/configmaps/hot-env"),
                                "{head}"
                            );
                            (
                                200,
                                serde_json::json!({"apiVersion":"v1","kind":"ConfigMap",
                                "metadata":{"name":"hot-env","resourceVersion":if step == 1 {"42"} else {"43"}},
                                "data": if step == 1 {serde_json::json!({"OLD_IDENTITY":"stale"})} else {serde_json::json!(desired)}}),
                            )
                        }
                        2 => {
                            assert!(
                                head.starts_with("PATCH ") && head.contains("/configmaps/hot-env"),
                                "{head}"
                            );
                            let body: serde_json::Value =
                                serde_json::from_slice(&bytes[offset..offset + length])
                                    .expect("patch JSON");
                            assert_eq!(body["metadata"]["resourceVersion"], "42");
                            let mut expected = serde_json::json!(desired);
                            expected["OLD_IDENTITY"] = serde_json::Value::Null;
                            assert_eq!(body["data"], expected);
                            if conflict {
                                (
                                    409,
                                    serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Conflict","message":"stale writer","code":409}),
                                )
                            } else {
                                (
                                    200,
                                    serde_json::json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"hot-env","resourceVersion":"43"},"data":desired}),
                                )
                            }
                        }
                        _ => unreachable!(),
                    };
                    let body = serde_json::to_vec(&reply).expect("reply");
                    stream.write_all(format!("HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.expect("headers");
                    stream.write_all(&body).await.expect("reply");
                }
            });
            drop(rustls::crypto::ring::default_provider().install_default());
            let config = kube::Config::new(format!("http://{address}").parse().expect("URI"));
            let runtime = runtime(kube::Client::try_from(config).expect("client"));
            let snapshot = shared_types::AppEnvSnapshot {
                deployment_uid: Some("deployment-owned".into()),
                deployment_version: Some("10".into()),
                resource_name: Some("hot-env".into()),
                resource_version: Some("42".into()),
                env: std::collections::HashMap::from([("OLD_IDENTITY".into(), "stale".into())]),
            };
            let result = runtime
                .update_env_configmap_if_version("hot", &target, &snapshot)
                .await;
            if conflict {
                assert!(
                    matches!(result, Err(ContainerRuntimeError::Conflict(_))),
                    "{result:?}"
                );
            } else {
                result.expect("committed and read back");
            }
            server.await.expect("contract assertions");
        }
    }

    #[tokio::test]
    async fn agent_lookup_preserves_api_failure_instead_of_not_found() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut request = Vec::new();
                let mut buffer = [0u8; 2048];
                loop {
                    let count = stream.read(&mut buffer).await.expect("request");
                    assert!(count > 0, "request closed before complete headers");
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(offset) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                    {
                        let head = String::from_utf8_lossy(&request[..offset]);
                        assert!(head.starts_with("GET "), "{head}");
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().expect("length"))
                            })
                            .unwrap_or(0);
                        while request.len() < offset + 4 + length {
                            let count = stream.read(&mut buffer).await.expect("body");
                            assert!(count > 0);
                            request.extend_from_slice(&buffer[..count]);
                        }
                        break;
                    }
                }
                let body = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","reason":"Forbidden","message":"lookup forbidden","code":403}"#;
                let response = format!(
                    "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.expect("reply");
            }
        });
        drop(rustls::crypto::ring::default_provider().install_default());
        let config = kube::Config::new(format!("http://{address}").parse().expect("uri"));
        let runtime = runtime(kube::Client::try_from(config).expect("client"));
        let result = runtime
            .get_container_info_inner("lookup-error", &ServiceType::WebAgentRunner)
            .await;
        server.abort();
        assert!(
            result.is_err(),
            "API failure must not become successful absence: {result:?}"
        );
    }

    #[tokio::test]
    async fn captured_delete_sends_uid_and_version_and_stops_on_conflict() {
        use shared_types::{AppDeletionSnapshot, AppResourceIdentity, AppResourceKind};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 2048];
            let (offset, length) = loop {
                let n = stream.read(&mut buffer).await.expect("request");
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(offset) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&bytes[..offset]);
                    let mut request_line = head
                        .lines()
                        .next()
                        .expect("request line")
                        .split_whitespace();
                    assert_eq!(request_line.next(), Some("DELETE"));
                    let uri = request_line.next().expect("URI");
                    assert_eq!(
                        uri.split('?').next(),
                        Some(
                            "/api/v1/namespaces/review-test/persistentvolumeclaims/claimed-storage"
                        )
                    );
                    let length = head
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|length| length.trim().parse::<usize>().expect("length"))
                        })
                        .expect("content length");
                    break (offset + 4, length);
                }
            };
            while bytes.len() < offset + length {
                let n = stream.read(&mut buffer).await.expect("body");
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
            }
            let body: serde_json::Value =
                serde_json::from_slice(&bytes[offset..offset + length]).expect("JSON");
            assert_eq!(body["preconditions"]["uid"], "storage-uid");
            assert_eq!(body["preconditions"]["resourceVersion"], "42");
            let response = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","reason":"Conflict","message":"new storage use claim","code":409}"#;
            stream.write_all(format!("HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.expect("response");
        });
        drop(rustls::crypto::ring::default_provider().install_default());
        let config = kube::Config::new(format!("http://{address}").parse().expect("uri"));
        let runtime = runtime(kube::Client::try_from(config).expect("client"));
        let snapshot = AppDeletionSnapshot {
            app_id: "claimed-app".into(),
            operation_id: "delete-old".into(),
            resources: vec![AppResourceIdentity {
                kind: AppResourceKind::PersistentVolumeClaim,
                name: "claimed-storage".into(),
                uid: "storage-uid".into(),
                resource_version: Some("42".into()),
            }],
        };
        assert!(matches!(
            runtime.delete_captured(&snapshot, true).await,
            Err(ContainerRuntimeError::Conflict(_))
        ));
        server.await.expect("adapter assertions");
    }

    #[tokio::test]
    async fn agent_pvc_ensure_creates_workspace_without_deletion() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        drop(rustls::crypto::ring::default_provider().install_default());
        let config = kube::Config::new(format!("http://{address}").parse().expect("URI"));
        let runtime = runtime(kube::Client::try_from(config).expect("client"));
        let family = ServiceType::WebAgentRunner;
        let expected_name = runtime
            .workspace_pvc_name("retained-agent", &family)
            .expect("PVC name");
        let expected_family = family.to_string();
        let server = tokio::spawn(async move {
            for index in 0..2 {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut bytes = Vec::new();
                let mut buffer = [0u8; 2048];
                let (head, offset, length) = loop {
                    let count = stream.read(&mut buffer).await.expect("request");
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(offset) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&bytes[..offset]).into_owned();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().expect("length"))
                            })
                            .unwrap_or(0);
                        break (head, offset + 4, length);
                    }
                };
                while bytes.len() < offset + length {
                    let count = stream.read(&mut buffer).await.expect("body");
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                }
                let (status, body) = if index == 0 {
                    assert!(head.starts_with("GET "), "{head}");
                    assert!(head.contains(&expected_name), "{head}");
                    (
                        404,
                        serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","message":"not found","code":404}),
                    )
                } else {
                    assert!(
                        head.starts_with("POST "),
                        "ensure may create but must never delete: {head}"
                    );
                    let mut object: serde_json::Value =
                        serde_json::from_slice(&bytes[offset..offset + length]).expect("PVC");
                    assert_eq!(object["metadata"]["name"], expected_name);
                    assert_eq!(
                        object["metadata"]["labels"]["service_type"],
                        expected_family
                    );
                    object["metadata"]["uid"] = "retained-agent-pvc".into();
                    object["metadata"]["resourceVersion"] = "1".into();
                    (201, object)
                };
                let body = body.to_string();
                stream.write_all(format!("HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.expect("response");
            }
        });
        runtime
            .ensure_workspace_pvc("retained-agent", &family, None)
            .await
            .expect("agent workspace creation is allowed");
        server.await.expect("API assertions");
    }

    #[tokio::test]
    async fn agent_pvc_destroy_is_rejected_before_any_api_request() {
        drop(rustls::crypto::ring::default_provider().install_default());
        let config = kube::Config::new("http://127.0.0.1:1".parse().expect("uri"));
        let runtime = runtime(kube::Client::try_from(config).expect("client"));
        let result = K8sPvcOps::destroy_workspace_pvc(
            &runtime,
            "protected-agent",
            &ServiceType::WebAgentRunner,
        )
        .await;
        assert!(
            matches!(result, Err(ContainerRuntimeError::ConfigurationError(message)) if message.contains("forbidden"))
        );
    }

    #[tokio::test]
    async fn storage_reuse_claim_sends_uid_and_resource_version_cas() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            let mut patched = false;
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut bytes = Vec::new();
                let mut buffer = [0u8; 2048];
                let (head, offset, length) = loop {
                    let n = stream.read(&mut buffer).await.expect("request");
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(offset) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&bytes[..offset]).to_string();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|length| length.trim().parse::<usize>().expect("length"))
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
                let (code, response) = if head.starts_with("PATCH ") {
                    let body: serde_json::Value =
                        serde_json::from_slice(&bytes[offset..offset + length]).expect("patch");
                    assert_eq!(body["metadata"]["uid"], "pvc-owned");
                    assert_eq!(body["metadata"]["resourceVersion"], "42");
                    assert!(
                        !body["metadata"]["annotations"]["rcoder.io/storage-use-operation"]
                            .as_str()
                            .expect("claim")
                            .is_empty()
                    );
                    patched = true;
                    (
                        200,
                        serde_json::json!({"apiVersion":"v1","kind":"PersistentVolumeClaim","metadata":{"name":"rcoder-app-claim-test-workspace","labels":{"service_type":"user-app"},"uid":"pvc-owned","resourceVersion":"43"}}),
                    )
                } else if head
                    .lines()
                    .next()
                    .expect("request line")
                    .contains("-data ")
                {
                    (
                        404,
                        serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","code":404,"message":"legacy volume absent"}),
                    )
                } else {
                    (
                        200,
                        serde_json::json!({"apiVersion":"v1","kind":"PersistentVolumeClaim","metadata":{"name":"rcoder-app-claim-test-workspace","labels":{"service_type":"user-app"},"uid":"pvc-owned","resourceVersion":"42"}}),
                    )
                };
                let body = response.to_string();
                stream.write_all(format!("HTTP/1.1 {code} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.expect("response");
            }
            assert!(patched);
        });
        drop(rustls::crypto::ring::default_provider().install_default());
        let config = kube::Config::new(format!("http://{address}").parse().expect("uri"));
        let runtime = runtime(kube::Client::try_from(config).expect("client"));
        runtime
            .claim_app_storage("claim-test")
            .await
            .expect("claim");
        server.await.expect("adapter assertions");
    }

    #[tokio::test]
    async fn application_operation_lease_excludes_purge_and_release_is_identity_bound() {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
        for (replaced, cancelled) in [(false, false), (true, false), (false, true)] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let address = listener.local_addr().expect("address");
            let server = tokio::spawn(async move {
                let mut winner = serde_json::Value::Null;
                for request_index in 0..if cancelled { 3 } else { 5 } {
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let mut bytes = Vec::new();
                    let mut buffer = [0u8; 2048];
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
                    let mut body: serde_json::Value = if length == 0 { serde_json::Value::Null } else {
                        serde_json::from_slice(&bytes[offset..offset + length]).expect("json")
                    };
                    assert!(head.contains("/leases"), "{head}");
                    let conflict = request_index == 1 || (request_index == 4 && replaced);
                    if request_index < 2 {
                        assert!(head.starts_with("POST "));
                        assert!(
                            !body["metadata"]["annotations"]["rcoder.io/legacy-operation-id"]
                                .as_str()
                                .expect("operation")
                                .is_empty()
                        );
                        assert!(body["metadata"]["annotations"]["rcoder.io/operation-id"].is_null(), "legacy token must not advertise a joinable durable operation");
                        assert_eq!(body["metadata"]["labels"]["rcoder.io/operation-app"], "writer-paused");
                        assert_eq!(body["metadata"]["labels"]["rcoder.io/operation-family"], ServiceType::Userapp.to_string());
                        assert!(!body["spec"]["holderIdentity"].as_str().expect("holder").is_empty());
                        body["metadata"]["uid"] = "lease-owner".into();
                        body["metadata"]["resourceVersion"] = "42".into();
                        if request_index == 0 { winner = body.clone(); }
                    } else if request_index == 2 || request_index == 3 {
                        // 2 = 第二次 acquire 的接管资格探测；3 = release 的
                        // 身份/实时 RV 重读。两者都回显持有者（renewTime 新鲜
                        // → 未过期 → 报 InProgress 而非抢占）。
                        assert!(head.starts_with("GET "));
                        assert!(head.contains("/leases/rcoder-operation-prod-writer-paused"));
                        assert_eq!(length, 0);
                        body = winner.clone();
                    } else {
                        assert!(head.starts_with("DELETE "));
                        assert_eq!(body["preconditions"]["uid"], "lease-owner");
                        assert_eq!(body["preconditions"]["resourceVersion"], "42");
                        body = serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Success","code":200});
                    }
                    let code = if conflict { 409 } else { 200 };
                    if conflict {
                        body = serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Conflict","message":"another operation owns this resource","code":409});
                    }
                    let body = body.to_string();
                    stream.write_all(format!("HTTP/1.1 {code} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.expect("response");
                }
            });
            drop(rustls::crypto::ring::default_provider().install_default());
            let config = kube::Config::new(format!("http://{address}").parse().expect("uri"));
            let runtime = runtime(kube::Client::try_from(config).expect("client"));
            let writer = runtime
                .acquire_application_operation("writer-paused", &ServiceType::Userapp)
                .await
                .expect("writer lease");
            let writer = if cancelled {
                drop(writer);
                None
            } else {
                Some(writer)
            };
            // The writer is paused after acquiring ownership and before its Deployment POST.
            // 6885fabd 起冲突读回回退 legacy-operation-id：legacy 持有者的令牌
            // 必须上报（Some），报 None 会让运维读成"没人持有却锁着"。
            assert!(matches!(
                runtime
                    .acquire_application_operation("writer-paused", &ServiceType::Userapp)
                    .await,
                Err(ContainerRuntimeError::OperationInProgress(ref operation))
                    if operation.app_id == "writer-paused"
                        && operation.service_type == ServiceType::Userapp
                        && operation.resource_name == "rcoder-operation-prod-writer-paused"
                        && operation.operation_id.is_some()
            ));
            if let Some(writer) = writer {
                let released = writer.release().await;
                assert_eq!(
                    released.is_err(),
                    replaced,
                    "old release cannot remove a replacement operation"
                );
            }
            server.await.expect("adapter assertions");
        }
        }).await.expect("bounded operation lease wire scenario");
    }

    /// Real kube client and real generation writer; only the API server is a local
    /// deterministic adapter. No kubeconfig/in-cluster discovery or cluster access.
    #[tokio::test]
    async fn conditional_writers_have_one_winner_and_compensate_only_owned_config() {
        run_conditional_writers(false, false).await;
    }

    #[tokio::test]
    async fn lost_commit_response_does_not_delete_potentially_active_config() {
        run_conditional_writers(true, false).await;
    }

    #[tokio::test]
    async fn concurrent_creates_never_compensate_the_winning_deployment() {
        run_conditional_writers(false, true).await;
    }

    async fn run_conditional_writers(lose_success_response: bool, create: bool) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let version = Arc::new(AtomicU64::new(42));
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let captured = captured.clone();
                let version = version.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let mut buf = [0u8; 4096];
                    let (head, offset, length) = loop {
                        let n = stream.read(&mut buf).await.expect("read request");
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buf[..n]);
                        if let Some(offset) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&bytes[..offset]).to_string();
                            let length = head
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().expect("length"))
                                })
                                .unwrap_or(0);
                            break (head, offset + 4, length);
                        }
                    };
                    while bytes.len() < offset + length {
                        let n = stream.read(&mut buf).await.expect("body");
                        assert!(n > 0);
                        bytes.extend_from_slice(&buf[..n]);
                    }
                    let line = head.lines().next().expect("request line");
                    let method = line.split_whitespace().next().expect("method");
                    let path = line.split_whitespace().nth(1).expect("path");
                    if method == "GET" {
                        // 换代回收的 list 探测（label 选择器查询）：并发用例无历史代，
                        // 返回空列表保持 mock 确定性（真实回收行为见专项用例）
                        let kind = if path.contains("/configmaps") {
                            "ConfigMapList"
                        } else {
                            "SecretList"
                        };
                        let payload = serde_json::to_vec(
                            &serde_json::json!({"apiVersion":"v1","kind":kind,"items":[]}),
                        )
                        .expect("serialize");
                        let headers = format!(
                            "HTTP/1.1 200 Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            payload.len()
                        );
                        stream.write_all(headers.as_bytes()).await.expect("headers");
                        stream.write_all(&payload).await.expect("response");
                        return;
                    }
                    let body: serde_json::Value =
                        serde_json::from_slice(&bytes[offset..offset + length]).expect("JSON");
                    captured.lock().expect("capture").push((
                        method.to_owned(),
                        path.to_owned(),
                        body.clone(),
                    ));
                    let mut reply = body;
                    let mut status = 200;
                    if (method == "PUT" || method == "POST") && path.contains("/deployments") {
                        if create {
                            assert!(reply["metadata"]["resourceVersion"].is_null());
                        } else {
                            assert_eq!(reply["metadata"]["resourceVersion"], "42");
                        }
                        if version
                            .compare_exchange(42, 43, Ordering::SeqCst, Ordering::SeqCst)
                            .is_err()
                        {
                            status = 409;
                            reply = serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","message":"conflict","reason":"Conflict","code":409});
                        } else {
                            reply["metadata"]["uid"] = "deployment-owned".into();
                            reply["metadata"]["resourceVersion"] = "43".into();
                            if lose_success_response {
                                // The API committed, but the client never receives its receipt.
                                return;
                            }
                        }
                    } else if method == "POST" {
                        let name = reply["metadata"]["name"].as_str().expect("name").to_owned();
                        reply["metadata"]["uid"] = format!("owned-{name}").into();
                        reply["metadata"]["resourceVersion"] = "1".into();
                        status = 201;
                    } else if method == "DELETE" {
                        assert!(path.contains("/configmaps/") || path.contains("/secrets/"));
                        assert!(
                            reply["preconditions"]["uid"]
                                .as_str()
                                .expect("UID required")
                                .starts_with("owned-")
                        );
                        reply = serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Success","code":200});
                    }
                    let payload = serde_json::to_vec(&reply).expect("serialize");
                    let headers = format!(
                        "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        payload.len()
                    );
                    stream.write_all(headers.as_bytes()).await.expect("headers");
                    stream.write_all(&payload).await.expect("response");
                });
            }
        });
        drop(rustls::crypto::ring::default_provider().install_default());
        drop(rustls::crypto::ring::default_provider().install_default());
        let config = kube::Config::new(format!("http://{addr}").parse().expect("URI"));
        let rt = runtime(kube::Client::try_from(config).expect("local client"));
        let params = ContainerCreateParams::builder()
            .project_id("app-review")
            .service_type(ServiceType::Userapp)
            .image_override("runtime:test")
            .build();
        let (a, b) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(
                rt.write_app_generation(
                    "app-review",
                    &params,
                    if create { None } else { Some("42") },
                    None
                ),
                rt.write_app_generation(
                    "app-review",
                    &params,
                    if create { None } else { Some("42") },
                    None
                )
            )
        })
        .await
        .expect("bounded conditional writes");
        if lose_success_response {
            assert!(
                a.is_err() && b.is_err(),
                "one conflict and one uncertain commit"
            );
        } else {
            assert_ne!(
                a.is_ok(),
                b.is_ok(),
                "only one version-42 update may commit"
            );
        }
        let seen = requests.lock().expect("requests");
        assert_eq!(
            seen.iter()
                .filter(|(method, _, _)| method == "DELETE")
                .count(),
            2
        );
        let names: std::collections::HashSet<_> = seen
            .iter()
            .filter(|(method, path, _)| method == "POST" && !path.contains("/deployments"))
            .map(|(_, _, body)| body["metadata"]["name"].as_str().expect("name"))
            .collect();
        assert_eq!(
            names.len(),
            4,
            "each writer owns distinct config and secret"
        );
        server.abort();
    }
    /// 换代提交成功后回收被取代的历史配置代（真实调用链）：按 app 选择器 list
    /// 探测、活跃引用保留、仅 `ua-{app_id}-` 历史代按 UID 前置条件删除。
    #[tokio::test]
    async fn generation_commit_reclaims_superseded_config_generations() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("address");
        let active: Arc<Mutex<(String, String)>> =
            Arc::new(Mutex::new((String::new(), String::new())));
        let seen: Arc<Mutex<Vec<(String, String, serde_json::Value)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let (captured_seen, captured_active) = (seen.clone(), active.clone());
        let server = tokio::spawn(async move {
            for step in 0..7usize {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let (seen, active) = (captured_seen.clone(), captured_active.clone());
                let mut bytes = Vec::new();
                let mut buf = [0u8; 4096];
                let (head, offset, length) = loop {
                    let n = stream.read(&mut buf).await.expect("read request");
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(offset) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&bytes[..offset]).to_string();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().expect("length"))
                            })
                            .unwrap_or(0);
                        break (head, offset + 4, length);
                    }
                };
                while bytes.len() < offset + length {
                    let n = stream.read(&mut buf).await.expect("body");
                    assert!(n > 0);
                    bytes.extend_from_slice(&buf[..n]);
                }
                let line = head.lines().next().expect("request line");
                let method = line.split_whitespace().next().expect("method").to_owned();
                let path = line.split_whitespace().nth(1).expect("path").to_owned();
                let body: serde_json::Value = if length == 0 {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&bytes[offset..offset + length]).expect("JSON")
                };
                seen.lock()
                    .expect("seen")
                    .push((method.clone(), path.clone(), body.clone()));
                let stale_cm = "ua-app-review-stale0000-env";
                let stale_sec = "ua-app-review-stale0000-sec";
                let (status, reply) = match step {
                    0 => {
                        assert!(
                            method == "POST" && path.contains("/configmaps"),
                            "{method} {path}"
                        );
                        let name = body["metadata"]["name"]
                            .as_str()
                            .expect("cm name")
                            .to_owned();
                        active.lock().expect("active").0 = name.clone();
                        (
                            201,
                            serde_json::json!({"metadata":{"name":name,"uid":"owned-active-cm","resourceVersion":"1"}}),
                        )
                    }
                    1 => {
                        assert!(
                            method == "POST" && path.contains("/secrets"),
                            "{method} {path}"
                        );
                        let name = body["metadata"]["name"]
                            .as_str()
                            .expect("sec name")
                            .to_owned();
                        active.lock().expect("active").1 = name.clone();
                        (
                            201,
                            serde_json::json!({"metadata":{"name":name,"uid":"owned-active-sec","resourceVersion":"1"}}),
                        )
                    }
                    2 => {
                        assert!(
                            method == "POST" && path.contains("/deployments"),
                            "{method} {path}"
                        );
                        (
                            201,
                            serde_json::json!({"metadata":{"uid":"deployment-owned","resourceVersion":"43"}}),
                        )
                    }
                    3 => {
                        assert!(
                            method == "GET" && path.contains("/configmaps"),
                            "{method} {path}"
                        );
                        assert!(
                            path.contains("app-review") && path.contains("rcoder-app-manager"),
                            "reclaim must list by app selector: {path}"
                        );
                        (
                            200,
                            serde_json::json!({"apiVersion":"v1","kind":"ConfigMapList","items":[
                                {"metadata":{"name": active.lock().expect("active").0.clone(), "uid":"owned-active-cm"}},
                                {"metadata":{"name": stale_cm, "uid":"stale-cm-uid"}}]}),
                        )
                    }
                    4 => {
                        assert!(
                            method == "DELETE"
                                && path
                                    .split('?')
                                    .next()
                                    .is_some_and(|p| p.ends_with(stale_cm)),
                            "must delete only superseded env: {method} {path}"
                        );
                        assert_eq!(body["preconditions"]["uid"], "stale-cm-uid");
                        (
                            200,
                            serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Success","code":200}),
                        )
                    }
                    5 => {
                        assert!(
                            method == "GET" && path.contains("/secrets"),
                            "{method} {path}"
                        );
                        (
                            200,
                            serde_json::json!({"apiVersion":"v1","kind":"SecretList","items":[
                                {"metadata":{"name": active.lock().expect("active").1.clone(), "uid":"owned-active-sec"}},
                                {"metadata":{"name": stale_sec, "uid":"stale-sec-uid"}}]}),
                        )
                    }
                    6 => {
                        assert!(
                            method == "DELETE"
                                && path
                                    .split('?')
                                    .next()
                                    .is_some_and(|p| p.ends_with(stale_sec)),
                            "must delete only superseded secret: {method} {path}"
                        );
                        assert_eq!(body["preconditions"]["uid"], "stale-sec-uid");
                        (
                            200,
                            serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Success","code":200}),
                        )
                    }
                    _ => unreachable!(),
                };
                let payload = serde_json::to_vec(&reply).expect("serialize");
                let headers = format!(
                    "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                stream.write_all(headers.as_bytes()).await.expect("headers");
                stream.write_all(&payload).await.expect("response");
            }
        });
        drop(rustls::crypto::ring::default_provider().install_default());
        let config = kube::Config::new(format!("http://{addr}").parse().expect("URI"));
        let rt = runtime(kube::Client::try_from(config).expect("local client"));
        let params = ContainerCreateParams::builder()
            .project_id("app-review")
            .service_type(ServiceType::Userapp)
            .image_override("runtime:test")
            .build();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            rt.write_app_generation("app-review", &params, None, None)
                .await
                .expect("generation commit");
        })
        .await
        .expect("bounded reclaim flow");
        server.await.expect("wire assertions");
        let (active_cm, active_sec) = active.lock().expect("active").clone();
        let seen = seen.lock().expect("seen");
        assert!(
            !active_cm.is_empty() && !active_sec.is_empty(),
            "staged generation names captured"
        );
        // 活跃引用从未被删除；DELETE 仅命中两个历史代
        assert!(seen.iter().all(|(method, path, _)| {
            !(method == "DELETE" && (path.contains(&active_cm) || path.contains(&active_sec)))
        }));
        assert_eq!(
            seen.iter()
                .filter(|(method, _, _)| method == "DELETE")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn deletion_waits_for_disappearance_and_stops_on_untrusted_observation() {
        use shared_types::{AppDeletionSnapshot, AppResourceIdentity, AppResourceKind};
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            for outcome in ["gone", "query-error", "replacement"] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
                let address = listener.local_addr().expect("address");
                let server = tokio::spawn(async move {
                    let steps = if outcome == "gone" { 5 } else { 3 };
                    for step in 0..steps {
                        let (mut stream, _) = listener.accept().await.expect("accept");
                        let mut bytes = Vec::new();
                        let mut chunk = [0u8; 4096];
                        let (head, offset, length) = loop {
                            let count = stream.read(&mut chunk).await.expect("read request");
                            assert!(count > 0, "incomplete request");
                            bytes.extend_from_slice(&chunk[..count]);
                            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
                                let length = head.lines().find_map(|line| {
                                    let (key, value) = line.split_once(':')?;
                                    key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().expect("length"))
                                }).unwrap_or(0);
                                break (head, end + 4, length);
                            }
                        };
                        while bytes.len() < offset + length {
                            let count = stream.read(&mut chunk).await.expect("read body");
                            assert!(count > 0, "incomplete body");
                            bytes.extend_from_slice(&chunk[..count]);
                        }
                        let first = head.lines().next().expect("request line");
                        let later_resource = step >= 3;
                        let path = if later_resource { "/api/v1/namespaces/review-test/services/next-service" }
                            else { "/apis/apps/v1/namespaces/review-test/deployments/original" };
                        let method = if step == 0 || step == 3 { "DELETE" } else { "GET" };
                        assert!(first.starts_with(&format!("{method} {path}")), "unexpected request {first}");
                        let (code, body) = if method == "DELETE" {
                            let request: serde_json::Value = serde_json::from_slice(&bytes[offset..offset + length]).expect("delete JSON");
                            assert_eq!(request["preconditions"]["uid"], if later_resource { "service-uid" } else { "original-uid" });
                            assert_eq!(request["preconditions"]["resourceVersion"], "7");
                            assert_eq!(request["propagationPolicy"], "Foreground");
                            (200, serde_json::json!({"kind":"Status","apiVersion":"v1","status":"Success","code":200}))
                        } else if step == 1 || (step == 2 && outcome == "replacement") {
                            // First observation is still terminating: another resource
                            // cannot be deleted before a later authoritative observation.
                            (200, serde_json::json!({"kind":"Deployment","apiVersion":"apps/v1","metadata": {
                                "name":"original", "uid": if step == 1 { "original-uid" } else { "replacement-uid" },
                                "resourceVersion":"8", "deletionTimestamp":"2026-01-01T00:00:00Z"
                            }}))
                        } else if step == 2 && outcome == "query-error" {
                            (503, serde_json::json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"ServiceUnavailable","message":"injected observation failure","code":503}))
                        } else {
                            (404, serde_json::json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","message":"captured resource absent","code":404}))
                        };
                        let body = body.to_string();
                        let response = format!("HTTP/1.1 {code} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                        stream.write_all(response.as_bytes()).await.expect("response");
                        stream.shutdown().await.expect("close");
                    }
                });
                drop(rustls::crypto::ring::default_provider().install_default());
                let config = kube::Config::new(format!("http://{address}").parse().expect("URI"));
                let runtime = runtime(kube::Client::try_from(config).expect("client"));
                let snapshot = AppDeletionSnapshot {
                    app_id: "app".into(), operation_id: "delete-original".into(),
                    resources: vec![
                        AppResourceIdentity { kind: AppResourceKind::Deployment, name: "original".into(), uid: "original-uid".into(), resource_version: Some("7".into()) },
                        AppResourceIdentity { kind: AppResourceKind::Service, name: "next-service".into(), uid: "service-uid".into(), resource_version: Some("7".into()) },
                    ],
                };
                let result = runtime.delete_captured(&snapshot, false).await;
                match outcome {
                    "gone" => result.expect("all captured resources confirmed absent"),
                    "replacement" => assert!(matches!(result, Err(ContainerRuntimeError::Conflict(_)))),
                    _ => match result {
                        Err(ContainerRuntimeError::K8sError(message)) => assert!(message.contains("observe captured resource deletion"), "must propagate the failed observation rather than trying another delete: {message}"),
                        other => panic!("expected observation failure, got {other:?}"),
                    },
                }
                server.await.expect("wire assertions");
            }
        }).await.expect("bounded deletion scenarios");
    }
}
