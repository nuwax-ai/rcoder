use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn runtime(client: Client) -> KubernetesRuntime {
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
        event_counters: Arc::new(crate::runtime::k8s_event_publisher::PublisherCounters::default()),
    }
}

/// 读一个完整 HTTP 请求（head + body），返回 (请求行起的 head, body)。
async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
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
    (head, bytes[offset..offset + length].to_vec())
}

async fn write_reply(stream: &mut tokio::net::TcpStream, code: u16, body: &serde_json::Value) {
    let body = body.to_string();
    stream
            .write_all(
                format!(
                    "HTTP/1.1 {code} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .expect("response");
}

#[tokio::test]
async fn idle_builder_drain_failure_still_stops_only_captured_resources() {
    drop(rustls::crypto::ring::default_provider().install_default());
    for replaced in [false, true] {
        tokio::time::timeout(std::time::Duration::from_secs(6), async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    let mut writes = Vec::new();
                    let mut pod_reads = 0;
                    let mut drained = false;
                    loop {
                        let Ok(Ok((mut stream, _))) = tokio::time::timeout(
                            std::time::Duration::from_secs(1), listener.accept()).await else { break };
                        let (head, body) = read_request(&mut stream).await;
                        let first = head.lines().next().unwrap();
                        assert!(!first.contains("persistentvolumeclaims"), "{first}");
                        let path = first.split_whitespace().nth(1).unwrap();
                        let name = path.split('?').next().unwrap().rsplit('/').next().unwrap();
                        let mut code = 200;
                        let reply = if path.contains("/exec?") {
                            drained = true;
                            code = 400; // Management failure cannot block physical retirement.
                            serde_json::json!({"kind":"Status","apiVersion":"v1","code":400,"reason":"BadRequest","message":"owner unavailable"})
                        } else if first.starts_with("DELETE ") {
                            assert!(drained, "capture and bounded drain precede deletion");
                            let params: serde_json::Value = serde_json::from_slice(&body).unwrap();
                            assert_eq!(params["preconditions"]["uid"], format!("original-{name}"));
                            assert_eq!(params["preconditions"]["resourceVersion"], "7");
                            assert!(!path.contains("/pods/"), "must not force-delete a possibly replaced Pod");
                            writes.push(path.to_owned());
                            if replaced { code = 409; }
                            serde_json::json!({"kind":"Status","apiVersion":"v1","code":code,
                                "status":if replaced { "Failure" } else { "Success" },"message":"captured delete"})
                        } else if path.contains("/pods/") {
                            pod_reads += 1;
                            if pod_reads > 1 {
                                code = 404;
                                serde_json::json!({"kind":"Status","apiVersion":"v1","code":404,"reason":"NotFound","message":"gone"})
                            } else {
                                serde_json::json!({"kind":"Pod","apiVersion":"v1","metadata":{
                                    "name":name,"uid":"original-pod","resourceVersion":"7",
                                    "ownerReferences":[{"apiVersion":"apps/v1","kind":"StatefulSet",
                                        "name":"rcoder-app-builder-review","uid":"original-rcoder-app-builder-review","controller":true}]}})
                            }
                        } else {
                            assert!(first.starts_with("GET "), "{first}");
                            assert!(!drained, "never refresh deletion targets after drain");
                            serde_json::json!({"kind":if path.contains("/statefulsets/") { "StatefulSet" } else { "Service" },
                                "apiVersion":if path.contains("/statefulsets/") { "apps/v1" } else { "v1" },
                                "metadata":{"name":name,"uid":format!("original-{name}"),"resourceVersion":"7"}})
                        };
                        write_reply(&mut stream, code, &reply).await;
                    }
                    assert!(drained);
                    assert_eq!(writes.len(), if replaced { 1 } else { 3 });
                    assert!(writes[0].contains("/statefulsets/"));
                });
                let client = Client::try_from(Config::new(format!("http://{address}").parse().unwrap())).unwrap();
                let result = runtime(client).stop_container_by_identifier_inner("review", &ServiceType::UserappBuilder).await;
                assert_eq!(result.is_err(), replaced, "{result:?}");
                server.await.unwrap();
            }).await.unwrap();
    }
}

/// create 中途确定性失败（claim builder storage 被 API server 403 拒——线上
/// 0.1.264 实测形态）时，operation lease 必须被显式释放：Err 只 drop 会把
/// ConfigMap 锁留在集群里，该 app 的后续 ensure 全部 409（5 把 builder 锁
/// 全残留的事故）。断言核心 = 失败后到达的锁 DELETE 请求。
#[tokio::test]
async fn create_container_releases_operation_lease_when_create_fails() {
    claim_failure(Some(403), true, false).await;
}

#[tokio::test]
async fn create_container_retains_lease_when_claim_response_is_lost() {
    claim_failure(None, false, false).await;
}

#[tokio::test]
async fn create_container_retains_lease_when_claim_returns_server_error() {
    claim_failure(Some(500), false, false).await;
}

#[tokio::test]
async fn cached_builder_propagates_service_write_failure() {
    claim_failure(Some(403), true, true).await;
    claim_failure(None, false, true).await;
}

#[tokio::test]
async fn storage_claim_retries_only_same_uid_conflicts() {
    storage_claim_conflict(false, 409, 2).await;
}

#[tokio::test]
async fn storage_claim_replacement_is_never_patched() {
    storage_claim_conflict(true, 409, 1).await;
}

#[tokio::test]
async fn storage_claim_forbidden_is_not_retried() {
    storage_claim_conflict(false, 403, 1).await;
}

#[tokio::test]
async fn storage_claim_conflict_budget_is_bounded() {
    storage_claim_conflict(false, 409, 4).await;
}

async fn storage_claim_conflict(replaced: bool, code: u16, expected_patches: usize) {
    drop(rustls::crypto::ring::default_provider().install_default());
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut patches = Vec::new();
            loop {
                let accepted =
                    tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept())
                        .await;
                let Ok(Ok((mut stream, _))) = accepted else {
                    break;
                };
                let (head, body) = read_request(&mut stream).await;
                assert!(head.contains("persistentvolumeclaims"), "{head}");
                if head.starts_with("GET ") {
                    let uid = if replaced && !patches.is_empty() {
                        "replacement"
                    } else {
                        "original"
                    };
                    write_reply(
                        &mut stream,
                        200,
                        &serde_json::json!({
                            "apiVersion":"v1","kind":"PersistentVolumeClaim",
                            "metadata":{"name":"rcoder-app-builder-review-workspace",
                                "uid":uid,"resourceVersion":(patches.len()+1).to_string(),
                                "labels":{"service_type":"user-app-builder"}}
                        }),
                    )
                    .await;
                } else {
                    assert!(head.starts_with("PATCH "), "{head}");
                    let patch: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(patch["metadata"]["uid"], "original");
                    assert_eq!(
                        patch["metadata"]["resourceVersion"],
                        (patches.len() + 1).to_string()
                    );
                    patches.push(patch);
                    if expected_patches == 2 && patches.len() == 2 {
                        write_reply(
                            &mut stream,
                            200,
                            &serde_json::json!({
                                "apiVersion":"v1","kind":"PersistentVolumeClaim",
                                "metadata":{"uid":"original","resourceVersion":"3"}
                            }),
                        )
                        .await;
                    } else {
                        write_reply(
                            &mut stream,
                            code,
                            &serde_json::json!({
                                "apiVersion":"v1","kind":"Status","status":"Failure",
                                "reason":"Failure","code":code,"message":"injected rejection"
                            }),
                        )
                        .await;
                    }
                }
            }
            patches
        });
        let client =
            Client::try_from(Config::new(format!("http://{address}").parse().unwrap())).unwrap();
        let context = shared_types::UserAppExecutionContext {
            app_id: "review".into(),
            lifecycle_id: "lifecycle-one".into(),
            operation_id: "admitted-operation".into(),
            executor_id: "executor-one".into(),
            request_fingerprint: "ab".repeat(32),
        };
        let result = runtime(client)
            .claim_builder_storage_with_context("review", Some(&context))
            .await;
        assert_eq!(result.is_ok(), expected_patches == 2, "{result:?}");
        let patches = server.await.unwrap();
        assert_eq!(patches.len(), expected_patches);
        for patch in &patches {
            assert_eq!(
                patch["metadata"]["annotations"]["rcoder.io/storage-use-operation"],
                "admitted-operation"
            );
            assert_eq!(
                patch["metadata"]["annotations"]["rcoder.io/lifecycle-id"],
                "lifecycle-one"
            );
            assert_eq!(
                patch["metadata"]["annotations"], patches[0]["metadata"]["annotations"],
                "retry must keep the original operation identity"
            );
        }
    })
    .await
    .expect("claim regression must finish within its total budget");
}

async fn claim_failure(code: Option<u16>, should_release: bool, cached: bool) {
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server =
            tokio::spawn(async move {
                let mut lease_object = serde_json::Value::Null;
                // 前 4 个请求：acquire POST Lease → ensure 探测 GET PVC →
                // claim 读 GET PVC → claim PATCH PVC；缓存场景继续 GET/PATCH Service。
                for _ in 0..if cached { 6 } else { 4 } {
                    let (mut stream, _) = listener.accept().await.expect("accept");
                    let (head, body) = read_request(&mut stream).await;
                    if head.starts_with("POST ") && head.contains("/leases") {
                        let mut object: serde_json::Value =
                            serde_json::from_slice(&body).expect("acquire body");
                        object["metadata"]["uid"] = "lease-owner".into();
                        object["metadata"]["resourceVersion"] = "42".into();
                        lease_object = object.clone();
                        write_reply(&mut stream, 200, &object).await;
                    } else if head.starts_with("GET ") && head.contains("persistentvolumeclaims") {
                        write_reply(
                            &mut stream,
                            200,
                            &serde_json::json!({
                                "apiVersion":"v1","kind":"PersistentVolumeClaim",
                                "metadata":{
                                    "name":"rcoder-app-builder-errclaim-workspace",
                                    "labels":{"service_type":"user-app-builder"},
                                    "uid":"pvc-owned","resourceVersion":"42"}
                            }),
                        )
                        .await;
                    } else if head.starts_with("PATCH ") && head.contains("persistentvolumeclaims")
                    {
                        if cached {
                            write_reply(
                                &mut stream,
                                200,
                                &serde_json::json!({
                                    "apiVersion":"v1", "kind":"PersistentVolumeClaim",
                                    "metadata":{"uid":"pvc-owned", "resourceVersion":"43"}
                                }),
                            )
                            .await;
                            continue;
                        }
                        // Receiving the write and closing without a response models an
                        // unknown server outcome, not a rejected request.
                        let Some(code) = code else {
                            continue;
                        };
                        write_reply(
                            &mut stream,
                            code,
                            &serde_json::json!({
                                "apiVersion":"v1","kind":"Status","status":"Failure",
                                "reason":"Failure","code":code,
                                "message":"persistentvolumeclaims is forbidden: cannot patch"
                            }),
                        )
                        .await;
                    } else if cached && head.starts_with("GET ") && head.contains("/services/") {
                        write_reply(
                            &mut stream,
                            200,
                            // 归属校验（validate_builder_service）四要素齐全：
                            // identifier/service-type 家族标签 + 全量 selector +
                            // 非 headless（无 clusterIP）；uid/RV 供端口收敛
                            // patch 的 precondition——夹具残缺会在到达注入失败
                            // 前先撞归属 Conflict，遮蔽被测传播语义。
                            &serde_json::json!({
                                "apiVersion":"v1", "kind":"Service",
                                "metadata":{
                                    "name":"rcoder-app-builder-errclaim-svc",
                                    "uid":"svc-owned","resourceVersion":"42",
                                    "labels":{
                                        "app.kubernetes.io/name":"user-app-builder",
                                        "app.kubernetes.io/instance":"errclaim",
                                        "app.kubernetes.io/version":"v1",
                                        "app.kubernetes.io/component":"agent",
                                        "app.kubernetes.io/managed-by":"rcoder-runtime",
                                        "app.kubernetes.io/part-of":"rcoder",
                                        "rcoder.io/app-id":"errclaim",
                                        "rcoder.io/identifier":"errclaim",
                                        "rcoder.io/service-type":"user-app-builder"
                                    }
                                },
                                "spec":{
                                    "selector":{
                                        "app.kubernetes.io/name":"user-app-builder",
                                        "app.kubernetes.io/instance":"errclaim",
                                        "app.kubernetes.io/managed-by":"rcoder-runtime",
                                        "rcoder.io/identifier":"errclaim"
                                    },
                                    "ports":[]
                                }
                            }),
                        )
                        .await;
                    } else if cached && head.starts_with("PATCH ") && head.contains("/services/") {
                        if let Some(code) = code {
                            write_reply(&mut stream, code, &serde_json::json!({
                                "apiVersion":"v1", "kind":"Status", "status":"Failure",
                                "reason":"Failure", "code":code, "message":"service patch failed"
                            })).await;
                        }
                    } else {
                        panic!("unexpected request: {head}");
                    }
                }
                // 后续请求：失败路径的锁释放 = GET Lease（重读身份/实时 RV）
                // → DELETE Lease（本测试的修复断言核心；回归时（Err 不释放）
                // 此处 accept 超时，saw_release 保持 false）。
                let mut saw_release = false;
                if let Ok(Ok((mut stream, _))) =
                    tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept()).await
                {
                    let (head, _) = read_request(&mut stream).await;
                    assert!(
                        head.starts_with("GET ")
                            && head.contains("/leases/rcoder-operation-builder-errclaim"),
                        "{head}"
                    );
                    write_reply(&mut stream, 200, &lease_object).await;
                    if let Ok(Ok((mut stream, _))) =
                        tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                            .await
                    {
                        let (head, body) = read_request(&mut stream).await;
                        assert!(
                            head.starts_with("DELETE ")
                                && head.contains("/leases/rcoder-operation-builder-errclaim"),
                            "{head}"
                        );
                        let preconditions: serde_json::Value =
                            serde_json::from_slice(&body).expect("release body");
                        assert_eq!(preconditions["preconditions"]["uid"], "lease-owner");
                        assert_eq!(preconditions["preconditions"]["resourceVersion"], "42");
                        write_reply(
                            &mut stream,
                            200,
                            &serde_json::json!({"apiVersion":"v1","kind":"Status",
                            "status":"Success","code":200}),
                        )
                        .await;
                        saw_release = true;
                    }
                }
                assert_eq!(
                    saw_release, should_release,
                    "lease release must depend on confirmed rejection, not just Err"
                );
            });
        drop(rustls::crypto::ring::default_provider().install_default());
        let config = Config::new(format!("http://{address}").parse().expect("uri"));
        let runtime = runtime(Client::try_from(config).expect("client"));
        if cached {
            runtime.pod_cache.write().await.insert(
                "errclaim".into(),
                CachedPod {
                    info: RuntimeContainerInfo {
                        container_id: "pod-existing".into(),
                        container_name: "rcoder-app-builder-errclaim-0".into(),
                        container_ip: "10.0.0.1".into(),
                        status: ContainerRuntimeStatus::Running,
                        created_at: chrono::Utc::now(),
                        env_vars: None,
                        service_type: Some(ServiceType::UserappBuilder),
                        project_id: None,
                        user_id: None,
                        pod_id: None,
                        app_id: Some("errclaim".into()),
                        workload_uid: None,
                    },
                    service_type: ServiceType::UserappBuilder,
                    cached_at: std::time::Instant::now(),
                },
            );
        }
        let params = ContainerCreateParams::builder()
            .project_id("errclaim")
            .user_id("u-lease")
            .service_type(ServiceType::UserappBuilder)
            .storage_size("10Gi")
            .build();
        let result = runtime.create_container(params).await;
        let error = result.expect_err("create must propagate the injected mutation failure");
        assert!(
            error.to_string().contains(if cached {
                "patch agent service"
            } else {
                "claim builder storage"
            }),
            "{error}"
        );
        server.await.expect("adapter assertions");
    })
    .await
    .expect("builder rejection exchange exceeded deadline");
}

/// write_app_resources 的阶段进度包装：claim 拒绝/未知结果都必须以
/// CreationAborted 上抛并携带累计进度——确定性拒绝（403）携带整操作
/// 安全结束证明，未知类（5xx）不得携带（上层保持围栏）。
async fn app_creation_claim_outcome(patch_code: u16) -> container_runtime_api::CreationProgress {
    drop(rustls::crypto::ring::default_provider().install_default());
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let pvc_reply = serde_json::json!({
                "apiVersion":"v1","kind":"PersistentVolumeClaim",
                "metadata":{"name":"rcoder-app-progclaim-workspace",
                    "uid":"pvc-uid","resourceVersion":"7",
                    "labels":{"service_type":"user-app"}}
            });
            // ① ensure GET（active 复用）② claim 读 GET ③ claim PATCH
            for step in 0..3 {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let (head, _body) = read_request(&mut stream).await;
                assert!(head.contains("persistentvolumeclaims"), "{head}");
                if head.starts_with("GET ") {
                    write_reply(&mut stream, 200, &pvc_reply).await;
                } else if step == 2 {
                    write_reply(
                        &mut stream,
                        patch_code,
                        &serde_json::json!({
                            "apiVersion":"v1","kind":"Status","status":"Failure",
                            "reason":"Injected","code":patch_code,"message":"injected"
                        }),
                    )
                    .await;
                } else {
                    panic!("unexpected request before claim patch: {head}");
                }
            }
        });
        let client =
            Client::try_from(Config::new(format!("http://{address}").parse().unwrap())).unwrap();
        let params = ContainerCreateParams::builder()
            .project_id("progclaim")
            .user_id("u-prog")
            .service_type(ServiceType::Userapp)
            .storage_size("10Gi")
            .build();
        let error = runtime(client)
            .write_app_resources("progclaim", &params, None, None, Default::default(), None)
            .await
            .expect_err("claim outcome must abort creation");
        server.await.expect("scripted exchange completed");
        let ContainerRuntimeError::CreationAborted { progress, .. } = &error else {
            panic!("creation failure must carry progress, got: {error}");
        };
        progress.clone()
    })
    .await
    .expect("progress regression must finish within its budget")
}

#[tokio::test]
async fn app_creation_claim_rejection_proves_safe_finish() {
    let progress = app_creation_claim_outcome(403).await;
    assert_eq!(
        progress.failed_at,
        container_runtime_api::CreationStage::StorageClaim
    );
    assert!(progress.definitive_rejection);
    assert!(progress.safe_finish_ok(), "{progress:?}");
    assert!(
        progress
            .retained_idempotent_resources
            .iter()
            .any(|item| item.contains("workspace pvc ensured")),
        "{progress:?}"
    );
    assert!(
        progress
            .retained_idempotent_resources
            .iter()
            .any(|item| item.contains("storage-claim annotations may persist")),
        "部分认领可能性必须显式记录，不得冒充零变更: {progress:?}"
    );
}

#[tokio::test]
async fn app_creation_unknown_claim_outcome_retains_fence() {
    let progress = app_creation_claim_outcome(503).await;
    assert_eq!(
        progress.failed_at,
        container_runtime_api::CreationStage::StorageClaim
    );
    assert!(!progress.definitive_rejection);
    assert!(!progress.safe_finish_ok(), "{progress:?}");
}
