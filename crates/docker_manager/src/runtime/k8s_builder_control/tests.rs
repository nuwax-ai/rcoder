use super::*;

#[cfg(test)]
fn workload_identity(
    sts: &StatefulSet,
    context: &UserAppExecutionContext,
) -> Result<AppResourceIdentity> {
    workload_identity_with_binding(sts, context, None, false)
}

/// 语义契约服务器场景：object/ready_pod 为集群状态模板，标志位决定动作
/// 分支，`patched` 记录成功的 STS PATCH（此后单对象 GET 反映动作后 replicas）。
#[cfg(test)]
#[derive(Clone)]
struct ContractScenario {
    object: serde_json::Value,
    ready_pod: serde_json::Value,
    reject_patch: bool,
    restart: bool,
    wake: bool,
    image_roll: Option<String>,
    runtime_workspace: Option<String>,
    replaced: bool,
    patched: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    recorder: std::sync::Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
}

#[cfg(test)]
impl ContractScenario {
    fn post_action_replicas(&self) -> i64 {
        if self.wake { 1 } else { 0 }
    }

    fn post_action_sts(&self) -> serde_json::Value {
        let mut observed = self.object.clone();
        observed["spec"]["replicas"] = serde_json::json!(self.post_action_replicas());
        if let Some(image) = &self.image_roll {
            observed["spec"]["template"]["spec"]["containers"][0]["image"] =
                serde_json::json!(image);
        }
        if let Some(root) = &self.runtime_workspace {
            observed["spec"]["template"]["spec"]["containers"][0]["env"] = serde_json::json!([
                {"name":"USER_ID","value":"owner"},
                {"name":"TOKEN","valueFrom":{"secretKeyRef":{"name":"credentials","key":"token"}}},
                {"name":"APP_CLI_RUNTIME_WORKSPACE","value":root}
            ]);
        }
        observed
    }
}

#[cfg(test)]
async fn handle_contract_connection(mut stream: tokio::net::TcpStream, scenario: ContractScenario) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    let mut headers = String::new();
    let mut body = Vec::new();
    loop {
        let n = stream.read(&mut buffer).await.expect("read");
        if n == 0 {
            // 连接池探测/复用半关闭——无请求，丢弃该连接
            headers.clear();
            break;
        }
        bytes.extend_from_slice(&buffer[..n]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]).to_string();
            let length = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse::<usize>().expect("length"))
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                headers = head;
                body = bytes[end + 4..end + 4 + length].to_vec();
                break;
            }
        }
    }
    if headers.is_empty() {
        return;
    }
    let first = headers.lines().next().expect("request line");
    assert!(
        !first.contains("persistentvolumeclaims") && !first.contains("/services"),
        "storage/service mutation is fenced: {first}"
    );
    scenario.recorder.lock().expect("recorder").push((
        first.to_string(),
        headers.clone(),
        String::from_utf8_lossy(&body).to_string(),
    ));

    let is_watch = first.contains("watch=true");
    let (method, path_query) = {
        let mut parts = first.split_whitespace();
        (
            parts.next().expect("method").to_string(),
            parts.next().expect("path").to_string(),
        )
    };
    let path = path_query.split('?').next().expect("path").to_string();

    // WATCH 流：chunked 开头 + 按场景投递一个触发事件后保持连接
    if is_watch {
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .await
            .expect("watch header");
        let event = if path.ends_with("/statefulsets") {
            // STS 流：投递当前身份（identity 闸门验证；replicas 已按场景推进）
            let observed = scenario.post_action_sts();
            Some(serde_json::json!({"type":"MODIFIED","object":observed}))
        } else if scenario.restart || scenario.wake {
            // restart/wake：新 ready Pod 上线（name=agent_pod_name 派生名——
            // watcher 按名单投递；uid ready-pod 与旧 pod-original 区分新旧）
            let mut fresh = scenario.ready_pod.clone();
            fresh["metadata"]["name"] = serde_json::json!("rcoder-app-builder-app-0");
            fresh["metadata"]["uid"] = serde_json::json!("ready-pod");
            Some(serde_json::json!({"type":"ADDED","object":fresh}))
        } else {
            // stop：Pod 消失完成（DELETED——kube-runtime ListWatch 只对
            // 已入册对象投递 Delete，LIST 必须先含旧 Pod）
            let mut gone = scenario.ready_pod.clone();
            gone["metadata"]["name"] = serde_json::json!("rcoder-app-builder-app-0");
            gone["metadata"]["uid"] = serde_json::json!("pod-original");
            Some(serde_json::json!({"type":"DELETED","object":gone}))
        };
        if let Some(event) = event {
            let payload = format!("{}\n", event);
            let chunk = format!("{:x}\r\n{}\r\n", payload.len(), payload);
            stream
                .write_all(chunk.as_bytes())
                .await
                .expect("watch event");
        }
        let mut drain = [0u8; 512];
        loop {
            if stream.read(&mut drain).await.unwrap_or(0) == 0 {
                break;
            }
        }
        return;
    }

    let single_sts = path.ends_with("/statefulsets/builder");
    let list_sts = path.ends_with("/statefulsets");
    let single_pod = {
        let tail = path.rsplit('/').next().unwrap_or("");
        path.contains("/pods/") && !tail.is_empty()
    };
    let list_pods = path.ends_with("/pods");
    // 动作后状态：成功的 STS PATCH 之后，单对象 GET 反映推进的 replicas
    let patched = scenario.patched.load(std::sync::atomic::Ordering::SeqCst) > 0;

    let (code, response): (u16, serde_json::Value) = if method == "DELETE" {
        if scenario.reject_patch {
            // 写操作整体被拒（restart 的 DELETE 同 PATCH 一道受拒——
            // 拒绝必须传播为 RequestRejected，绝不静默成功）
            (
                403,
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Forbidden","message":"Delete denied","code":403}),
            )
        } else {
            (
                200,
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Success"}),
            )
        }
    } else if method == "PATCH" {
        if scenario.reject_patch {
            (
                403,
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Forbidden","message":"Patch denied","code":403}),
            )
        } else {
            scenario
                .patched
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let observed = scenario.post_action_sts();
            (200, observed)
        }
    } else if scenario.replaced && single_sts && !list_sts {
        let mut replacement = scenario.object.clone();
        replacement["metadata"]["uid"] = serde_json::json!("replacement-sts");
        (200, replacement)
    } else if single_sts {
        let mut observed = scenario.object.clone();
        if patched {
            observed = scenario.post_action_sts();
        }
        (200, observed)
    } else if list_sts {
        let observed = scenario.post_action_sts();
        (
            200,
            serde_json::json!({"apiVersion":"v1","kind":"StatefulSetList","metadata":{"resourceVersion":"8"},"items":[observed]}),
        )
    } else if single_pod && path.ends_with("/builder-0") {
        // 旧 Pod（restart 删除前置的身份复核对象：name/uid/rv 必须与捕获一致）
        let mut old = scenario.ready_pod.clone();
        old["metadata"]["name"] = serde_json::json!("builder-0");
        old["metadata"]["uid"] = serde_json::json!("pod-original");
        old["metadata"]["resourceVersion"] = serde_json::json!("9");
        (200, old)
    } else if single_pod && scenario.wake && scenario.runtime_workspace.is_some() && !patched {
        (
            404,
            serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","code":404}),
        )
    } else if single_pod {
        if scenario.restart || scenario.wake {
            (200, scenario.ready_pod.clone())
        } else {
            (
                404,
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","message":"Pod absent","code":404}),
            )
        }
    } else if list_pods {
        let items = if scenario.restart || scenario.wake {
            // restart/wake（apply 侧 restart=true）：旧 Pod 已删——空集，
            // 新 ready Pod 由 watch ADDED 事件驱动（KR07：旧 Pod Ready 不能
            // 完成新 restart）
            serde_json::json!([])
        } else {
            // stop：旧 Pod 在册（DELETED 事件才能被 ListWatch 识别）
            let mut current = scenario.ready_pod.clone();
            current["metadata"]["name"] = serde_json::json!("rcoder-app-builder-app-0");
            current["metadata"]["uid"] = serde_json::json!("pod-original");
            serde_json::json!([current])
        };
        (
            200,
            serde_json::json!({"apiVersion":"v1","kind":"PodList","metadata":{"resourceVersion":"10"},"items":items}),
        )
    } else {
        (
            200,
            serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Success"}),
        )
    };
    let body = response.to_string();
    stream
        .write_all(
            format!(
                "HTTP/1.1 {code} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .expect("respond");
}

#[test]
fn physical_builder_start_accepts_running_agent_without_business_ready() {
    let context = UserAppExecutionContext {
        app_id: "app".into(),
        lifecycle_id: "life".into(),
        operation_id: "restart".into(),
        executor_id: "worker".into(),
        request_fingerprint: "a".repeat(64),
    };
    let workload = AppResourceIdentity {
        kind: AppResourceKind::StatefulSet,
        name: "builder".into(),
        uid: "sts-one".into(),
        resource_version: Some("1".into()),
    };
    let pod: Pod = serde_json::from_value(serde_json::json!({
            "metadata": {
                "name": "builder-0", "uid": "pod-two", "resourceVersion": "2",
                "creationTimestamp": "2026-01-01T00:00:00Z",
                "labels": {"rcoder.io/service-type": ServiceType::UserappBuilder.to_string(), "rcoder.io/identifier": "app"},
                "annotations": context.resource_metadata(),
                "ownerReferences": [{"apiVersion": "apps/v1", "kind": "StatefulSet", "name": "builder", "uid": "sts-one", "controller": true}]
            },
            "status": {
                "phase": "Running", "podIP": "10.0.0.9",
                "conditions": [{"type": "Ready", "status": "False"}],
                "containerStatuses": [{"name": "agent", "ready": false, "restartCount": 0, "image": "agent:latest", "imageID": "image-id", "containerID": "container-id", "state": {"running": {"startedAt": "2026-01-01T00:00:01Z"}}}]
            }
        }))
        .expect("pod fixture");
    assert!(builder_agent_running(&pod));
    assert!(matches!(
        ready_builder_info(&pod, &workload, &context, None, false),
        Ok(BuilderObservation::Ready(_))
    ));
    assert!(ready_builder_info(&pod, &workload, &context, None, true).is_err());
}

#[test]
fn bound_wake_accepts_api_defaults_but_rejects_changed_container_or_storage() {
    let expected = serde_json::json!({
        "containers": [{"name":"builder", "image":"builder:verified", "volumeMounts":[{"name":"workspace","mountPath":"/workspace"}]}],
        "volumes": [{"name":"workspace", "persistentVolumeClaim":{"claimName":"original-pvc"}}]
    });
    let mut actual = expected.clone();
    actual["restartPolicy"] = serde_json::json!("Always");
    actual["containers"][0]["imagePullPolicy"] = serde_json::json!("IfNotPresent");
    assert!(configured_fields_match(&expected, &actual));
    for replacement in [
        serde_json::json!({"containers": [{"name":"builder", "image":"builder:other"}]}),
        serde_json::json!({"containers": []}),
    ] {
        assert!(!configured_fields_match(&expected, &replacement));
    }
    actual["volumes"][0]["persistentVolumeClaim"]["claimName"] =
        serde_json::json!("replacement-pvc");
    assert!(!configured_fields_match(&expected, &actual));
    actual = expected.clone();
    actual["containers"]
        .as_array_mut()
        .expect("containers")
        .push(serde_json::json!({"name":"injected-sidecar"}));
    assert!(!configured_fields_match(&expected, &actual));
}

#[tokio::test]
async fn actual_stop_patch_is_fenced_and_never_touches_storage() {
    stop_api_contract(false, false, false, false).await;
    stop_api_contract(true, false, false, false).await;
    stop_api_contract(true, true, false, false).await;
}

#[tokio::test]
async fn actual_bound_wake_fences_uid_and_version_and_preserves_storage() {
    stop_api_contract(false, false, true, false).await;
    stop_api_contract(true, false, true, false).await;
    stop_api_contract(false, false, true, true).await;
}

#[tokio::test]
async fn bound_start_rolls_image_in_the_same_fenced_write() {
    stop_api_contract_with_image(false, false, true, false, Some("agent:new")).await;
}

async fn stop_api_contract(reject_patch: bool, restart: bool, wake: bool, replaced: bool) {
    stop_api_contract_with_image(reject_patch, restart, wake, replaced, None).await;
}

async fn stop_api_contract_with_image(
    reject_patch: bool,
    restart: bool,
    wake: bool,
    replaced: bool,
    image_roll: Option<&str>,
) {
    stop_api_contract_with_workspace(
        reject_patch,
        restart,
        wake,
        replaced,
        image_roll,
        None,
        false,
    )
    .await;
}

async fn stop_api_contract_with_workspace(
    reject_patch: bool,
    restart: bool,
    wake: bool,
    replaced: bool,
    image_roll: Option<&str>,
    runtime_workspace: Option<&str>,
    changed_version: bool,
) {
    let context = UserAppExecutionContext {
        app_id: "app".into(),
        lifecycle_id: "life".into(),
        operation_id: "stop".into(),
        executor_id: "worker".into(),
        request_fingerprint: "a".repeat(64),
    };
    let mut object = serde_json::json!({"apiVersion":"apps/v1","kind":"StatefulSet","metadata":{"name":"builder","uid":"sts-original","resourceVersion":"8","labels":{"rcoder.io/service-type":ServiceType::UserappBuilder.to_string(),"rcoder.io/identifier":"app"},"annotations":context.resource_metadata()},"spec":{"replicas":1,"serviceName":"builder","selector":{"matchLabels":{}},"template":{"metadata":{},"spec":{"containers":[]}}}});
    if wake {
        object["spec"]["replicas"] = serde_json::json!(0);
    }
    let mut ready_pod = serde_json::json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":"rcoder-app-builder-app-0","uid":"ready-pod","resourceVersion":"10","creationTimestamp":"2026-01-01T00:00:00Z","labels":{"rcoder.io/service-type":ServiceType::UserappBuilder.to_string(),"rcoder.io/identifier":"app"},"annotations":context.resource_metadata(),"ownerReferences":[{"apiVersion":"apps/v1","kind":"StatefulSet","name":"builder","uid":"sts-original","controller":true}]},"status":{"phase":"Running","podIP":"10.0.0.9","conditions":[{"type":"Ready","status":"True"}],"containerStatuses":[{"name":"agent","ready":true,"restartCount":0,"image":"agent:latest","imageID":"image-id","containerID":"container-id","state":{"running":{"startedAt":"2026-01-01T00:00:01Z"}}}]}});
    if wake {
        object["metadata"]["annotations"] = serde_json::json!({});
        object["spec"]["template"]["spec"]["containers"] = serde_json::json!([{"name":"agent","image":"agent:latest","env":[{"name":"USER_ID","value":"owner"}]}]);
        ready_pod["metadata"]["annotations"] = serde_json::json!({});
        ready_pod["spec"] = serde_json::json!({"containers":[{"name":"agent","env":[{"name":"USER_ID","value":"owner"}]}]});
    }
    if let Some(root) = runtime_workspace {
        object["spec"]["template"]["spec"]["containers"][0]["env"] = serde_json::json!([
            {"name":"APP_CLI_RUNTIME_WORKSPACE","value":"/home/user/app/code"},
            {"name":"USER_ID","value":"owner"},
            {"name":"APP_CLI_RUNTIME_WORKSPACE","valueFrom":{"configMapKeyRef":{"name":"obsolete","key":"root"}}},
            {"name":"TOKEN","valueFrom":{"secretKeyRef":{"name":"credentials","key":"token"}}}
        ]);
        ready_pod["spec"]["containers"][0]["env"]
            .as_array_mut()
            .expect("pod env")
            .push(serde_json::json!({"name":"APP_CLI_RUNTIME_WORKSPACE","value":root}));
    }
    if let Some(image) = image_roll {
        ready_pod["spec"]["containers"][0]["image"] = serde_json::json!(image);
    }
    let workload = workload_identity_with_binding(
        &serde_json::from_value(object.clone()).expect("workload"),
        &context,
        None,
        wake,
    )
    .expect("identity");
    if changed_version {
        object["metadata"]["resourceVersion"] = serde_json::json!("other-rv");
    }
    if replaced {
        object["metadata"]["uid"] = serde_json::json!("replacement-sts");
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let recorded =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String, String)>::new()));
    let scenario = ContractScenario {
        object: object.clone(),
        ready_pod: ready_pod.clone(),
        reject_patch,
        restart,
        wake,
        image_roll: image_roll.map(str::to_string),
        runtime_workspace: runtime_workspace.map(str::to_string),
        replaced,
        patched: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        recorder: recorded.clone(),
    };
    // 批次 B：语义分类服务器——按 method/path/query 分类应答（单对象 GET /
    // 集合 LIST / watch 流 / PATCH / DELETE），watch 流按场景投递触发事件
    //（wake/restart=新 ready Pod ADDED、stop=Pod DELETED）。每连接独立
    // task——watch 长连接不能阻塞 accept；连接 task detach（语义断言在
    // 主任务对 recorder 的复核里，服务器由 abort 终止）。
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(_) => return,
            };
            tokio::spawn(handle_contract_connection(stream, scenario.clone()));
        }
    });
    drop(rustls::crypto::ring::default_provider().install_default());
    let client = kube::Client::try_from(kube::Config::new(
        format!("http://{address}").parse().expect("uri"),
    ))
    .expect("client");
    let runtime = KubernetesRuntime {
        client,
        namespace: "review-test".into(),
        config: super::super::kubernetes_runtime::KubernetesRuntimeConfig {
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
        event_counters: std::sync::Arc::new(
            crate::runtime::k8s_event_publisher::PublisherCounters::default(),
        ),
    };
    let target = BuilderControlTarget {
        resource_binding: wake.then(|| shared_types::UserAppResourceBinding {
            app_id: "app".into(),
            lifecycle_id: "life".into(),
            service_type: ServiceType::UserappBuilder,
            physical_uid: "sts-original".into(),
            adopted_by_operation: "adopt".into(),
        }),
        context,
        workload: Some(workload),
        pod: restart.then(|| BuilderPodIdentity {
            name: "builder-0".into(),
            uid: "pod-original".into(),
            resource_version: "9".into(),
        }),
        restart_image: image_roll.map(str::to_string),
        restart_runtime_workspace: runtime_workspace.map(str::to_string),
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        let outcome = runtime
            .apply_builder_compute_mode(&target, restart || wake, wake)
            .await;
        if reject_patch || replaced || changed_version {
            assert!(
                matches!(outcome, Err(Error::RequestRejected(_))),
                "expected rejection, got: {outcome:?}"
            );
        } else if wake {
            assert_eq!(
                outcome.expect("wake").expect("ready").container_id,
                "ready-pod"
            );
        } else if restart {
            assert_eq!(
                outcome.expect("restart").expect("ready").container_id,
                "ready-pod"
            );
        } else {
            assert!(outcome.expect("stop").is_none());
        }
        // 语义复核：写操作必须按场景出现且携带物理前置；绝无存储/服务变更
        let requests = recorded.lock().expect("recorder").clone();
        assert!(!requests.is_empty(), "no requests were recorded");
        let mut sts_patch: Option<(String, String, String)> = None;
        let mut pod_delete: Option<(String, String, String)> = None;
        for request in &requests {
            let lower = request.0.to_ascii_lowercase();
            assert!(
                !lower.contains("persistentvolumeclaims") && !lower.contains("/services"),
                "storage/service mutation is fenced: {request:?}"
            );
            if lower.starts_with("patch ") && lower.contains("/statefulsets/builder") {
                assert!(
                    sts_patch.is_none(),
                    "workload patched at most once: {requests:?}"
                );
                sts_patch = Some(request.clone());
            }
            if lower.starts_with("delete ") && lower.contains("/pods/") {
                assert!(
                    pod_delete.is_none(),
                    "pod deleted at most once: {requests:?}"
                );
                pod_delete = Some(request.clone());
            }
        }
        if replaced || changed_version {
            // 替换的 STS 在任何写之前被拒——绝无写操作
            assert!(
                sts_patch.is_none() && pod_delete.is_none(),
                "replacement must be fenced before any write: {requests:?}"
            );
        } else if wake || !restart {
            let (first, headers, body) = sts_patch.expect("workload patch required").clone();
            assert!(first.contains("PATCH"), "recorded: {first}");
            assert!(
                body.contains("\"uid\":\"sts-original\""),
                "patch carries UID precondition: {body}"
            );
            assert!(
                body.contains("\"resourceVersion\":\"8\""),
                "patch carries version precondition: {body}"
            );
            assert!(
                body.contains(&format!("\"replicas\":{}", if wake { 1 } else { 0 })),
                "patch scales in the action direction: {body}"
            );
            assert!(
                !body.contains("volumes") && !body.contains("persistentVolumeClaim"),
                "patch must not touch storage: {body}"
            );
            if image_roll.is_some() || runtime_workspace.is_some() {
                assert!(headers.to_ascii_lowercase().contains("application/strategic-merge-patch+json"));
            }
            if let Some(root) = runtime_workspace {
                let patch: serde_json::Value = serde_json::from_str(&body).expect("patch json");
                let env = patch["spec"]["template"]["spec"]["containers"][0]["env"].as_array().expect("env list");
                assert_eq!(env[0], serde_json::json!({"$patch":"replace"}));
                assert_eq!(env.iter().filter(|entry| entry["name"] == "APP_CLI_RUNTIME_WORKSPACE").count(),1);
                assert!(env.iter().any(|entry| entry == &serde_json::json!({"name":"APP_CLI_RUNTIME_WORKSPACE","value":root})));
                assert!(env.iter().any(|entry| entry == &serde_json::json!({"name":"TOKEN","valueFrom":{"secretKeyRef":{"name":"credentials","key":"token"}}})));
                assert!(env.iter().any(|entry| entry == &serde_json::json!({"name":"USER_ID","value":"owner"})));
            }
            if let Some(image) = image_roll {
                assert!(
                    headers
                        .to_ascii_lowercase()
                        .contains("application/strategic-merge-patch+json"),
                    "image update must merge the named container: {headers}"
                );
                let patch: serde_json::Value = serde_json::from_str(&body).expect("patch json");
                assert_eq!(
                    patch["spec"]["template"]["spec"]["containers"][0]["image"],
                    image
                );
            }
            assert!(
                pod_delete.is_none(),
                "stop/wake never deletes the pod: {requests:?}"
            );
        } else {
            let (first, _, body) = pod_delete.expect("pod delete required").clone();
            assert!(first.contains("DELETE"), "recorded: {first}");
            assert!(
                first.contains("/pods/builder-0"),
                "delete targets the captured pod: {first}"
            );
            let params: serde_json::Value = serde_json::from_str(&body).expect("delete body json");
            assert_eq!(
                params["preconditions"]["uid"], "pod-original",
                "delete carries UID precondition: {body}"
            );
            assert_eq!(
                params["preconditions"]["resourceVersion"], "9",
                "delete carries version precondition: {body}"
            );
            assert!(
                sts_patch.is_none(),
                "restart never patches the workload: {requests:?}"
            );
        }
        // 服务器是常驻 accept 循环——显式中止并确认未 panic
        server.abort();
        match server.await {
            Err(join) if join.is_cancelled() => {}
            other => panic!("contract server task must end aborted: {other:?}"),
        }
    })
    .await
    .expect("total contract deadline");
}

#[test]
fn workload_and_pod_identity_reject_replacement_ownership() {
    let context = UserAppExecutionContext {
        app_id: "app".into(),
        lifecycle_id: "life".into(),
        operation_id: "stop".into(),
        executor_id: "worker".into(),
        request_fingerprint: "a".repeat(64),
    };
    let sts: StatefulSet = serde_json::from_value(serde_json::json!({"metadata":{"name":"builder", "uid":"sts-original", "resourceVersion":"8", "labels":{"rcoder.io/service-type":ServiceType::UserappBuilder.to_string(),"rcoder.io/identifier":"app"}, "annotations":context.resource_metadata()}})).expect("workload");
    let identity = workload_identity(&sts, &context).expect("identity");
    let mut replacement = context.clone();
    replacement.lifecycle_id = "replacement".into();
    assert!(workload_identity(&sts, &replacement).is_err());
    let mut pod: Pod = serde_json::from_value(serde_json::json!({"metadata":{"name":"builder-0","uid":"pod-original","resourceVersion":"9","ownerReferences":[{"apiVersion":"apps/v1","kind":"StatefulSet","name":"builder","uid":"sts-original","controller":true}]}})).expect("pod");
    assert!(pod_identity(&pod, &identity).is_ok());
    pod.metadata.owner_references.as_mut().expect("owners")[0].uid = "replacement-sts".into();
    assert!(pod_identity(&pod, &identity).is_err());
}

#[test]
fn stop_and_restart_requests_carry_physical_preconditions() {
    let workload = AppResourceIdentity {
        kind: AppResourceKind::StatefulSet,
        name: "builder".into(),
        uid: "original-sts".into(),
        resource_version: Some("17".into()),
    };
    let patch = stop_patch(&workload);
    assert_eq!(patch["metadata"]["uid"], "original-sts");
    assert_eq!(patch["metadata"]["resourceVersion"], "17");
    assert_eq!(patch["spec"]["replicas"], 0);
    let params = pod_delete_params(&BuilderPodIdentity {
        name: "builder-0".into(),
        uid: "original-pod".into(),
        resource_version: "24".into(),
    });
    let preconditions = params.preconditions.expect("preconditions");
    assert_eq!(preconditions.uid.as_deref(), Some("original-pod"));
    assert_eq!(preconditions.resource_version.as_deref(), Some("24"));
}

#[tokio::test]
async fn bound_start_converges_legacy_workspace_even_with_latest_image() {
    for image in [None, Some("agent:latest")] {
        stop_api_contract_with_workspace(
            false,
            false,
            true,
            false,
            image,
            Some("/home/user/app"),
            false,
        )
        .await;
    }
}

#[tokio::test]
async fn bound_workspace_restart_rejects_replaced_uid_or_version_before_writes() {
    stop_api_contract_with_workspace(
        false,
        false,
        true,
        true,
        None,
        Some("/home/user/app"),
        false,
    )
    .await;
    stop_api_contract_with_workspace(
        false,
        false,
        true,
        false,
        None,
        Some("/home/user/app"),
        true,
    )
    .await;
}

#[test]
fn restart_confirmation_requires_unique_literal_workspace_on_sts_and_actual_pod() {
    let pod = |env: serde_json::Value| {
        serde_json::from_value::<Pod>(
            serde_json::json!({"spec":{"containers":[{"name":"agent","env":env}]}}),
        )
        .unwrap()
    };
    let correct =
        pod(serde_json::json!([{"name":"APP_CLI_RUNTIME_WORKSPACE","value":"/home/user/app"}]));
    let old = pod(
        serde_json::json!([{"name":"APP_CLI_RUNTIME_WORKSPACE","value":"/home/user/app/code"}]),
    );
    let duplicate = pod(serde_json::json!([
        {"name":"APP_CLI_RUNTIME_WORKSPACE","value":"/home/user/app"},
        {"name":"APP_CLI_RUNTIME_WORKSPACE","value":"/home/user/app/code"}
    ]));
    let indirect = pod(
        serde_json::json!([{"name":"APP_CLI_RUNTIME_WORKSPACE","valueFrom":{"configMapKeyRef":{"name":"config","key":"root"}}}]),
    );
    let mut sts: StatefulSet = serde_json::from_value(serde_json::json!({"spec":{"serviceName":"builder","selector":{"matchLabels":{}},"template":{"spec":{"containers":[{"name":"agent"}]}}}})).unwrap();
    sts.spec.as_mut().unwrap().template.spec = correct.spec.clone();
    assert!(statefulset_agent_workspace_matches(
        &sts,
        Some("/home/user/app")
    ));
    assert!(pod_agent_workspace_matches(
        &correct,
        Some("/home/user/app")
    ));
    for invalid in [&old, &duplicate, &indirect] {
        assert!(!pod_agent_workspace_matches(
            invalid,
            Some("/home/user/app")
        ));
        sts.spec.as_mut().unwrap().template.spec = invalid.spec.clone();
        assert!(!statefulset_agent_workspace_matches(
            &sts,
            Some("/home/user/app")
        ));
    }
    assert!(
        pod_agent_workspace_matches(&old, None),
        "legacy intent retains old behavior"
    );
}
