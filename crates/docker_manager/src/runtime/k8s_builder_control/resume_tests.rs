//! Exercise the production bound-resume entry through an actual kube client.
use super::*;
use container_runtime_api::ContainerCreateParams;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) struct ApiState {
    pub(super) sts: serde_json::Value,
    pub(super) pod: serde_json::Value,
    pub(super) service: serde_json::Value,
    pub(super) requests: Vec<String>,
    replace_uid_on_second_sts_read: bool,
    sts_reads: usize,
    pub(super) advance_status_rv: bool,
    pub(super) allow_writes: bool,
    pub(super) lose_next_put_response: bool,
    pub(super) archives: std::collections::BTreeMap<String, serde_json::Value>,
    pub(super) puts: Vec<serde_json::Value>,
    pub(super) patches: Vec<serde_json::Value>,
    pub(super) pvc: serde_json::Value,
    pod_seed: serde_json::Value,
    pub(super) extra_pods: Vec<serde_json::Value>,
    resume_replicas_on_second_sts_read: bool,
    fail_pod_list: bool,
}

async fn connection(mut stream: tokio::net::TcpStream, state: Arc<Mutex<ApiState>>) {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream.read(&mut buffer).await.expect("read request");
        if count == 0 {
            return;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|length| length.trim().parse::<usize>().expect("content length"))
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                break;
            }
        }
    }
    let headers = String::from_utf8_lossy(&bytes);
    let first = headers.lines().next().expect("request line");
    let path = first.split_whitespace().nth(1).expect("path");
    let method = first.split_whitespace().next().expect("method");
    let end = bytes
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .expect("headers end");
    let request_body = &bytes[end + 4..];
    let watch = path.contains("watch=true");
    let resource_path = path.split('?').next().expect("resource path");
    let (status, reply, lose_response) = {
        let mut state = state.lock().expect("API state");
        state.requests.push(first.to_owned());
        assert!(
            state.allow_writes || first.starts_with("GET "),
            "bound running ensure must not mutate: {first}"
        );
        let mut status = 200;
        let mut lose_response = false;
        let reply = if method == "POST" && resource_path.ends_with("/secrets") {
            let mut object: serde_json::Value =
                serde_json::from_slice(request_body).expect("secret body");
            object["metadata"]["uid"] = "archive-original".into();
            object["metadata"]["resourceVersion"] = "20".into();
            let name = object["metadata"]["name"]
                .as_str()
                .expect("secret name")
                .to_owned();
            if let std::collections::btree_map::Entry::Vacant(entry) = state.archives.entry(name) {
                entry.insert(object.clone());
                object
            } else {
                status = 409;
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"AlreadyExists","message":"Archive already exists","code":409})
            }
        } else if resource_path.contains("/secrets/") {
            if let Some(archive) = state
                .archives
                .get(resource_path.rsplit('/').next().expect("secret name"))
            {
                archive.clone()
            } else {
                status = 404;
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","message":"Archive not found","code":404})
            }
        } else if resource_path.contains("/persistentvolumeclaims/") {
            state.pvc.clone()
        } else if method == "PATCH" && resource_path.contains("/statefulsets/") {
            let patch: serde_json::Value = serde_json::from_slice(request_body).expect("patch");
            assert_eq!(patch["metadata"]["uid"], state.sts["metadata"]["uid"]);
            assert_eq!(
                patch["metadata"]["resourceVersion"],
                state.sts["metadata"]["resourceVersion"]
            );
            if let Some(annotations) = patch["metadata"]["annotations"].as_object() {
                for (name, value) in annotations {
                    state.sts["metadata"]["annotations"][name] = value.clone();
                }
            }
            if let Some(containers) = patch["spec"]["template"]["spec"]["containers"].as_array() {
                for desired in containers {
                    let actual = state.sts["spec"]["template"]["spec"]["containers"]
                        .as_array_mut()
                        .expect("containers")
                        .iter_mut()
                        .find(|container| container["name"] == desired["name"])
                        .expect("named container");
                    if let Some(image) = desired.get("image") {
                        actual["image"] = image.clone();
                    }
                    if let Some(env) = desired.get("env").and_then(serde_json::Value::as_array) {
                        assert_eq!(
                            env[0],
                            serde_json::json!({"$patch":"replace"}),
                            "env update must be an explicit complete list"
                        );
                        actual["env"] = serde_json::Value::Array(env[1..].to_vec());
                    }
                }
            }
            state.sts["spec"]["replicas"] = patch["spec"]["replicas"].clone();
            let rv = state.sts["metadata"]["resourceVersion"]
                .as_str()
                .expect("RV")
                .parse::<u64>()
                .expect("numeric RV")
                + 1;
            state.sts["metadata"]["resourceVersion"] = rv.to_string().into();
            if patch["spec"]["replicas"] == 0 {
                state.pod = serde_json::Value::Null;
            } else {
                let mut pod = state.pod_seed.clone();
                pod["metadata"]["uid"] = "pod-updated".into();
                pod["spec"] = state.sts["spec"]["template"]["spec"].clone();
                state.pod = pod;
            }
            state.patches.push(patch);
            state.sts.clone()
        } else if method == "PUT" && resource_path.contains("/statefulsets/") {
            let mut object: serde_json::Value =
                serde_json::from_slice(request_body).expect("replacement");
            assert_eq!(object["metadata"]["uid"], state.sts["metadata"]["uid"]);
            assert_eq!(
                object["metadata"]["resourceVersion"],
                state.sts["metadata"]["resourceVersion"]
            );
            assert!(
                state.pod.is_null(),
                "old Pod must be gone before a template update"
            );
            object["metadata"]["resourceVersion"] = "10".into();
            state.puts.push(object.clone());
            state.sts = object.clone();
            let mut pod = state.pod_seed.clone();
            pod["metadata"]["uid"] = "pod-updated".into();
            pod["spec"] = object["spec"]["template"]["spec"].clone();
            state.pod = pod;
            lose_response = state.lose_next_put_response;
            state.lose_next_put_response = false;
            object
        } else if resource_path.contains("/statefulsets/") {
            state.sts_reads += 1;
            if state.resume_replicas_on_second_sts_read && state.sts_reads >= 2 {
                state.sts["spec"]["replicas"] = 1.into();
            }
            if state.advance_status_rv {
                let rv = state.sts["metadata"]["resourceVersion"]
                    .as_str()
                    .expect("RV")
                    .parse::<u64>()
                    .expect("RV number")
                    + 1;
                state.sts["metadata"]["resourceVersion"] = rv.to_string().into();
            }
            let mut object = state.sts.clone();
            if state.replace_uid_on_second_sts_read && state.sts_reads >= 2 {
                object["metadata"]["uid"] = serde_json::json!("foreign-controller");
            }
            object
        } else if resource_path.ends_with("/statefulsets") {
            if watch {
                serde_json::json!({"type":"MODIFIED","object":state.sts})
            } else {
                serde_json::json!({"apiVersion":"apps/v1","kind":"StatefulSetList","metadata":{"resourceVersion":"8"},"items":[state.sts]})
            }
        } else if resource_path.contains("/pods/") {
            if state.pod.is_null() {
                status = 404;
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","message":"Pod not found","code":404})
            } else {
                state.pod.clone()
            }
        } else if resource_path.ends_with("/pods") {
            if state.fail_pod_list {
                status = 500;
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"InternalError","message":"fixture read failure","code":500})
            } else if watch {
                serde_json::json!({"type":"ADDED","object":state.pod})
            } else {
                let mut items = if state.pod.is_null() {
                    vec![]
                } else {
                    vec![state.pod.clone()]
                };
                items.extend(state.extra_pods.clone());
                serde_json::json!({"apiVersion":"v1","kind":"PodList","metadata":{"resourceVersion":"9"},"items":items})
            }
        } else if resource_path.contains("/services/") {
            state.service.clone()
        } else {
            panic!("unexpected API request: {first}");
        };
        (status, reply, lose_response)
    };
    if lose_response {
        return;
    }
    let body = if watch {
        format!("{reply}\n")
    } else {
        reply.to_string()
    };
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        if status == 200 { "OK" } else { "Not Found" },
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("response");
}

fn runtime(client: kube::Client) -> KubernetesRuntime {
    KubernetesRuntime {
        client,
        namespace: "resume-contract".into(),
        config: super::super::kubernetes_runtime::KubernetesRuntimeConfig {
            namespace: "resume-contract".into(),
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
            execution_authority: "k8s:resume-test".into(),
        },
        pod_cache: Default::default(),
        subvolume_path_cache: Default::default(),
        event_publisher: Default::default(),
        event_counters: Default::default(),
    }
}

fn api_roundtrip(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(values) => {
            if values.get("value").is_some_and(|value| value == "") {
                values.remove("value");
            }
            if values.get("readOnly") == Some(&serde_json::Value::Bool(false)) {
                values.remove("readOnly");
            }
            for value in values.values_mut() {
                api_roundtrip(value);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                api_roundtrip(value);
            }
        }
        _ => {}
    }
}

pub(super) async fn fixture(
    mutation: Option<&str>,
) -> (
    KubernetesRuntime,
    ContainerCreateParams,
    Arc<Mutex<ApiState>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    drop(rustls::crypto::ring::default_provider().install_default());
    let client = kube::Client::try_from(kube::Config::new(
        format!("http://{}", listener.local_addr().expect("address"))
            .parse()
            .expect("URI"),
    ))
    .expect("kube client");
    let runtime = runtime(client);
    let original = UserAppExecutionContext {
        app_id: "app".into(),
        lifecycle_id: "life".into(),
        operation_id: "created".into(),
        executor_id: "worker".into(),
        request_fingerprint: "a".repeat(64),
    };
    let mut request = original.clone();
    request.operation_id = "ensure".into();
    // Keep the primary red case on the exact creation digest, so it exposes
    // the original token/default/order false conflict rather than another gate.
    if mutation == Some("release") || mutation == Some("fingerprint") {
        request.request_fingerprint = "b".repeat(64);
    }
    let mut params = ContainerCreateParams::builder()
        .project_id("app".to_string())
        .builder_app_id("app".to_string())
        .execution_context(request)
        .service_type(ServiceType::UserappBuilder)
        .build();
    let mut spec = runtime
        .build_agent_pod_spec("app", &ServiceType::UserappBuilder, &params)
        .expect("spec");
    let env = spec.containers[0].env.get_or_insert_default();
    env.retain(|entry| entry.name != "APP_CLI_DEPLOY_TOKEN");
    env.push(k8s_openapi::api::core::v1::EnvVar {
        name: "APP_CLI_DEPLOY_TOKEN".into(),
        value: Some("original-credential-never-replaced".into()),
        ..Default::default()
    });
    let mut sts = runtime
        .desired_builder_statefulset(&original, spec)
        .expect("StatefulSet");
    sts.metadata.uid = Some("sts-original".into());
    sts.metadata.resource_version = Some("8".into());
    params.resource_binding = Some(shared_types::UserAppResourceBinding {
        app_id: "app".into(),
        lifecycle_id: "life".into(),
        service_type: ServiceType::UserappBuilder,
        physical_uid: "sts-original".into(),
        adopted_by_operation: "adopt".into(),
    });
    let mut object = serde_json::to_value(&sts).expect("STS JSON");
    api_roundtrip(&mut object);
    object["spec"]["template"]["spec"]["containers"][0]["env"]
        .as_array_mut()
        .expect("env")
        .reverse();
    if let Some(mutation) = mutation {
        let pod_spec = &mut object["spec"]["template"]["spec"];
        match mutation {
            "pvc" => {
                pod_spec["volumes"][0]["persistentVolumeClaim"]["claimName"] = "foreign-pvc".into()
            }
            "mount" => {
                pod_spec["containers"][0]["volumeMounts"][0]["subPath"] = "another-app".into()
            }
            "domain" => {
                let domain = pod_spec["containers"][0]["env"]
                    .as_array_mut()
                    .expect("env")
                    .iter_mut()
                    .find(|entry| entry["name"] == runtime_supervisor::domain::DOMAIN_ENV)
                    .expect("domain");
                domain["value"] = serde_json::json!({"authority":"old-api-authority","volume":"another-application-volume"}).to_string().into();
            }
            "release" => {
                pod_spec["containers"][0]["image"] = "old-release:1".into();
                pod_spec["containers"][0]["env"]
                    .as_array_mut()
                    .expect("env")
                    .push(serde_json::json!({"name":"APPLICATION_OWNED_SETTING","value":"keep"}));
            }
            "uid" | "fingerprint" => {}
            _ => panic!("unknown mutation"),
        }
    }
    let pod_name = runtime
        .agent_pod_name("app", &ServiceType::UserappBuilder)
        .expect("pod name");
    let name = sts.metadata.name.as_deref().expect("STS name");
    let pod = serde_json::json!({"apiVersion":"v1","kind":"Pod", "metadata":{
        "name":pod_name,"uid":"pod-original","resourceVersion":"9","creationTimestamp":"2026-01-01T00:00:00Z",
        "labels":object["spec"]["template"]["metadata"]["labels"],"annotations":original.resource_metadata(),
        "ownerReferences":[{"apiVersion":"apps/v1","kind":"StatefulSet","name":name,"uid":"sts-original","controller":true}]},
        "spec":object["spec"]["template"]["spec"],
        "status":{"phase":"Running","podIP":"10.0.0.9","conditions":[{"type":"Ready","status":"True"}],
            "containerStatuses":[{"name":"agent","ready":true,"restartCount":0,"image":"image","imageID":"image-id","containerID":"container-id","state":{"running":{"startedAt":"2026-01-01T00:00:00Z"}}}]}});
    let service_name = runtime
        .agent_service_name("app", &ServiceType::UserappBuilder)
        .expect("service name");
    let mut service = super::super::k8s_service::agent_service_object(
        "resume-contract",
        &service_name,
        "app",
        &ServiceType::UserappBuilder,
    );
    service.metadata.uid = Some("service-original".into());
    service.metadata.resource_version = Some("7".into());
    // The production deploy-host feature registers the apiserver's allocated
    // NodePorts. Model that response rather than leaving every nodePort unset.
    // The loopback fixture really listens on this port; TCP probes can connect
    // without manufacturing readiness or changing the production code path.
    let published_port = i32::from(listener.local_addr().expect("address").port());
    for port in service
        .spec
        .as_mut()
        .expect("service spec")
        .ports
        .as_mut()
        .expect("service ports")
    {
        port.node_port = Some(published_port);
    }
    let claim_name =
        object["spec"]["template"]["spec"]["volumes"][0]["persistentVolumeClaim"]["claimName"]
            .as_str()
            .expect("PVC name");
    let pvc = serde_json::json!({"apiVersion":"v1","kind":"PersistentVolumeClaim","metadata":{
        "name":claim_name,"uid":"pvc-original","resourceVersion":"5","annotations":original.resource_metadata()},
        "spec":{"accessModes":["ReadWriteOnce"],"resources":{"requests":{"storage":"1Gi"}}},"status":{"phase":"Bound"}});
    let state = Arc::new(Mutex::new(ApiState {
        sts: object,
        pod_seed: pod.clone(),
        extra_pods: vec![],
        resume_replicas_on_second_sts_read: false,
        fail_pod_list: false,
        pod,
        pvc,
        allow_writes: false,
        lose_next_put_response: false,
        archives: Default::default(),
        puts: vec![],
        patches: vec![],
        service: serde_json::to_value(service).expect("service"),
        requests: vec![],
        replace_uid_on_second_sts_read: mutation == Some("uid"),
        sts_reads: 0,
        advance_status_rv: false,
    }));
    let served = state.clone();
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.expect("accept");
            tokio::spawn(connection(stream, served.clone()));
        }
    });
    (runtime, params, state, server)
}

#[tokio::test]
async fn stopped_observation_requires_retained_zero_controller_and_all_owned_pods_gone() {
    use container_runtime_api::AgentContainerRuntime as _;
    for case in [
        "running",
        "pending-without-pod",
        "stopped-without-receipt",
        "stopped-with-live-pod",
        "terminating-owned-pod-without-labels",
        "new-controller-uid",
        "rescaled-during-observation",
        "pod-list-read-error",
    ] {
        let (runtime, params, state, server) = fixture(None).await;
        let target = runtime
            .capture_builder_compute_with_binding(
                params.execution_context.as_ref().expect("context"),
                params.resource_binding.as_ref(),
                false,
            )
            .await
            .expect("capture existing target");
        {
            let mut state = state.lock().expect("state");
            state.sts_reads = 0;
            // No platform stop receipt is added: this is an actual external
            // stop observation, never an acknowledgement of our own write.
            if case != "running" {
                state.sts["spec"]["replicas"] = 0.into();
            }
            match case {
                "running" | "stopped-with-live-pod" => {}
                "pending-without-pod" => {
                    state.sts["spec"]["replicas"] = 1.into();
                    state.pod = serde_json::Value::Null;
                }
                "stopped-without-receipt" => state.pod = serde_json::Value::Null,
                "terminating-owned-pod-without-labels" => {
                    let mut terminating = state.pod_seed.clone();
                    terminating["metadata"]["name"] = "older-ordinal".into();
                    terminating["metadata"]["labels"] = serde_json::json!({});
                    terminating["metadata"]["deletionTimestamp"] = "2026-01-02T00:00:00Z".into();
                    terminating["metadata"]["ownerReferences"][0]["controller"] =
                        serde_json::Value::Null;
                    state.extra_pods.push(terminating);
                    state.pod = serde_json::Value::Null;
                }
                "new-controller-uid" => {
                    state.pod = serde_json::Value::Null;
                    state.replace_uid_on_second_sts_read = true;
                }
                "rescaled-during-observation" => {
                    state.pod = serde_json::Value::Null;
                    state.resume_replicas_on_second_sts_read = true;
                }
                "pod-list-read-error" => {
                    state.pod = serde_json::Value::Null;
                    state.fail_pod_list = true;
                }
                _ => unreachable!("enumerated case"),
            }
        }
        let observed = runtime.observe_builder_compute_stopped(&target).await;
        match case {
            "new-controller-uid" => assert!(
                matches!(observed, Err(Error::Conflict(_))),
                "replacement cannot count as the captured stopped target: {observed:?}"
            ),
            "pod-list-read-error" => assert!(
                observed.is_err(),
                "an actual read error must not become false/true: {observed:?}"
            ),
            "stopped-without-receipt" => assert!(observed.expect("stopped")),
            _ => assert!(!observed.expect("observed pending or executing"), "{case}"),
        }
        let state = state.lock().expect("state");
        assert!(
            state
                .requests
                .iter()
                .all(|request| request.starts_with("GET "))
        );
        assert_eq!(state.pvc["metadata"]["uid"], "pvc-original");
        assert!(state.puts.is_empty() && state.patches.is_empty());
        server.abort();
    }
}

#[tokio::test]
async fn bound_resume_reuses_api_normalized_configuration_and_original_token_twice() {
    let (runtime, mut params, state, server) = fixture(None).await;
    for operation in ["ensure", "ensure-again"] {
        params
            .execution_context
            .as_mut()
            .expect("context")
            .operation_id = operation.into();
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            runtime.resume_bound_builder(&params),
        )
        .await
        .expect("resume budget");
        assert!(
            outcome.is_ok(),
            "bound API-normalized reuse failed: {outcome:?}"
        );
        let (info, binding) = outcome.expect("resumed");
        assert_eq!(info.container_id, "pod-original");
        assert_eq!(info.workload_uid.as_deref(), Some("sts-original"));
        assert!(
            binding.is_none(),
            "reuse must not invent a new physical binding"
        );
    }
    let state = state.lock().expect("state");
    assert!(
        state
            .requests
            .iter()
            .all(|request| request.starts_with("GET "))
    );
    let token = state.sts["spec"]["template"]["spec"]["containers"][0]["env"]
        .as_array()
        .expect("env")
        .iter()
        .find(|entry| entry["name"] == "APP_CLI_DEPLOY_TOKEN")
        .expect("original token");
    assert_eq!(token["value"], "original-credential-never-replaced");
    server.abort();
}

#[tokio::test]
async fn bound_resume_keeps_a_running_previous_release_usable() {
    let (runtime, params, state, server) = fixture(Some("release")).await;
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        runtime.resume_bound_builder(&params),
    )
    .await
    .expect("resume budget");
    assert!(
        outcome.is_ok(),
        "ordinary release configuration blocked existing compute: {outcome:?}"
    );
    assert!(
        state
            .lock()
            .expect("state")
            .requests
            .iter()
            .all(|request| request.starts_with("GET "))
    );
    server.abort();
}

#[tokio::test]
async fn bound_resume_separates_platform_fingerprint_from_physical_ownership() {
    let (runtime, params, state, server) = fixture(Some("fingerprint")).await;
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        runtime.resume_bound_builder(&params),
    )
    .await
    .expect("resume budget");
    assert!(
        outcome.is_ok(),
        "a current platform digest is not foreign ownership: {outcome:?}"
    );
    assert!(
        state
            .lock()
            .expect("state")
            .requests
            .iter()
            .all(|request| request.starts_with("GET "))
    );
    server.abort();
}

#[tokio::test]
async fn bound_resume_fences_replaced_identity_and_workspace_before_any_write() {
    for mutation in ["uid", "pvc", "mount", "domain"] {
        let (runtime, params, state, server) = fixture(Some(mutation)).await;
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            runtime.resume_bound_builder(&params),
        )
        .await
        .expect("resume budget");
        assert!(
            matches!(outcome, Err(Error::RequestRejected(_))),
            "{mutation} must be fenced: {outcome:?}"
        );
        assert!(
            state
                .lock()
                .expect("state")
                .requests
                .iter()
                .all(|request| request.starts_with("GET "))
        );
        server.abort();
    }
}

#[tokio::test]
async fn stopped_builder_management_wake_sets_one_boot_intent_and_preserve_reuse_does_not_rewrite_it()
 {
    use container_runtime_api::AgentContainerRuntime as _;
    let (runtime, mut params, state, server) = fixture(None).await;
    state.lock().expect("state").allow_writes = true;
    let original = runtime
        .capture_builder_compute_with_binding(
            params.execution_context.as_ref().expect("context"),
            params.resource_binding.as_ref(),
            false,
        )
        .await
        .expect("source");
    assert!(
        runtime
            .apply_builder_control(&original, false)
            .await
            .expect("actual Stop")
            .is_none()
    );
    params.builder_startup_mode = shared_types::BuilderStartupMode::ManagementOnly;
    let outcome = runtime
        .resume_bound_builder(&params)
        .await
        .expect("management wake");
    assert_eq!(outcome.0.container_id, "pod-updated");
    let operation_id = params
        .execution_context
        .as_ref()
        .expect("context")
        .operation_id
        .clone();
    {
        let state = state.lock().expect("state");
        assert_eq!(state.patches.len(), 2, "one Stop then one management wake");
        let env = state.sts["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .expect("env");
        assert!(env.iter().any(
            |entry| entry["name"] == shared_types::APP_CLI_CONTROL_ONLY_ENV
                && entry["value"] == "true"
        ));
        assert!(env.iter().any(|entry| entry["name"]
            == shared_types::APP_CLI_MANAGEMENT_BOOT_ID_ENV
            && entry["value"] == operation_id));
    }
    params.builder_startup_mode = shared_types::BuilderStartupMode::PreserveBusinessIntent;
    params
        .execution_context
        .as_mut()
        .expect("context")
        .operation_id = "later-source-run".into();
    runtime
        .resume_bound_builder(&params)
        .await
        .expect("already running source reuse");
    assert_eq!(
        state.lock().expect("state").patches.len(),
        2,
        "a later run must not reset an existing owner's business intent"
    );
    server.abort();
}

#[tokio::test]
async fn legacy_missing_injection_and_historical_duplicate_token_do_not_block_running_reuse() {
    let (runtime, params, state, server) = fixture(None).await;
    {
        let mut state = state.lock().expect("state");
        let spec = &mut state.sts["spec"]["template"]["spec"];
        spec["volumes"]
            .as_array_mut()
            .expect("volumes")
            .retain(|volume| volume["name"] != "rcoder-platform-binding");
        spec["containers"][0]["volumeMounts"]
            .as_array_mut()
            .expect("mounts")
            .retain(|mount| mount["name"] != "rcoder-platform-binding");
        let env = spec["containers"][0]["env"].as_array_mut().expect("env");
        env.retain(|entry| {
            entry["name"] != runtime_supervisor::domain::DOMAIN_ENV
                && entry["name"] != "RCODER_PHYSICAL_POD_UID"
        });
        env.push(serde_json::json!({"name":"APP_CLI_DEPLOY_TOKEN","value":""}));
        state.pod["spec"] = state.sts["spec"]["template"]["spec"].clone();
    }
    let outcome = runtime.resume_bound_builder(&params).await;
    assert!(
        outcome.is_ok(),
        "old injection metadata is not an executing foreign owner: {outcome:?}"
    );
    assert!(
        state
            .lock()
            .expect("state")
            .requests
            .iter()
            .all(|request| request.starts_with("GET "))
    );
    server.abort();
}

#[tokio::test]
async fn running_reuse_accepts_controller_status_version_changes_without_writes() {
    let (runtime, params, state, server) = fixture(None).await;
    state.lock().expect("state").advance_status_rv = true;
    let outcome = runtime.resume_bound_builder(&params).await;
    assert!(
        outcome.is_ok(),
        "normal status versions do not replace data or the owner: {outcome:?}"
    );
    assert!(
        state
            .lock()
            .expect("state")
            .requests
            .iter()
            .all(|request| request.starts_with("GET "))
    );
    server.abort();
}
