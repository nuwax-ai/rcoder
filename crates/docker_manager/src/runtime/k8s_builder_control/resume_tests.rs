//! Exercise the production bound-resume entry through an actual kube client.
use super::*;
use container_runtime_api::ContainerCreateParams;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) struct ApiState {
    pub(super) sts: serde_json::Value,
    pub(super) pod: serde_json::Value,
    pub(super) service: serde_json::Value,
    pub(super) headless_service: serde_json::Value,
    pub(super) requests: Vec<String>,
    admission_fences: Vec<String>,
    replace_uid_on_second_sts_read: bool,
    sts_reads: usize,
    pub(super) advance_status_rv: bool,
    pub(super) allow_writes: bool,
    pub(super) lose_next_put_response: bool,
    lose_next_create_response: bool,
    pub(super) archives: std::collections::BTreeMap<String, serde_json::Value>,
    leases: std::collections::BTreeMap<String, serde_json::Value>,
    pub(super) puts: Vec<serde_json::Value>,
    pub(super) patches: Vec<serde_json::Value>,
    pub(super) pvc: serde_json::Value,
    pod_seed: serde_json::Value,
    pub(super) extra_pods: Vec<serde_json::Value>,
    resume_replicas_on_second_sts_read: bool,
    pub(super) fail_pod_list: bool,
    replace_uid_on_sts_read: Option<usize>,
    pub(super) replace_pvc_on_read: Option<usize>,
    pvc_reads: usize,
    fail_next_creation_receipt: bool,
}

fn without_retirement_fence(mut object: serde_json::Value) -> serde_json::Value {
    object["metadata"]
        .as_object_mut()
        .expect("metadata")
        .remove("resourceVersion");
    if let Some(annotations) = object["metadata"]["annotations"].as_object_mut() {
        annotations.remove("rcoder.io/agent-admission-fence");
    }
    object
}

fn apply_metadata_fence(object: &mut serde_json::Value, patch: &serde_json::Value) {
    assert_eq!(patch["metadata"]["uid"], object["metadata"]["uid"]);
    if let Some(version) = patch["metadata"].get("resourceVersion") {
        assert_eq!(version, &object["metadata"]["resourceVersion"]);
    }
    if let Some(annotations) = patch["metadata"]["annotations"].as_object() {
        for (key, value) in annotations {
            object["metadata"]["annotations"][key] = value.clone();
        }
    }
    let rv = object["metadata"]["resourceVersion"]
        .as_str()
        .expect("RV")
        .parse::<u64>()
        .expect("numeric RV")
        + 1;
    object["metadata"]["resourceVersion"] = rv.to_string().into();
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
        if method == "PATCH" {
            let patch: serde_json::Value =
                serde_json::from_slice(request_body).expect("PATCH body");
            if let Some(nonce) =
                patch["metadata"]["annotations"]["rcoder.io/agent-admission-fence"].as_str()
            {
                assert_eq!(patch.as_object().expect("patch object").len(), 1);
                assert_eq!(patch["metadata"].as_object().expect("metadata").len(), 3);
                assert!(patch["metadata"]["uid"].as_str().is_some());
                assert!(patch["metadata"]["resourceVersion"].as_str().is_some());
                assert_eq!(
                    patch["metadata"]["annotations"]
                        .as_object()
                        .expect("annotations")
                        .len(),
                    1
                );
                uuid::Uuid::parse_str(nonce).expect("actual admission nonce");
                state.admission_fences.push(first.to_owned());
            }
        }
        let reply = if method == "POST" && resource_path.ends_with("/leases") {
            let mut lease: serde_json::Value = serde_json::from_slice(request_body).expect("lease");
            let name = lease["metadata"]["name"].as_str().expect("name").to_owned();
            lease["metadata"]["uid"] = "lease-original".into();
            lease["metadata"]["resourceVersion"] = "1".into();
            state.leases.insert(name, lease.clone());
            lease
        } else if resource_path.contains("/leases/") {
            let name = resource_path.rsplit('/').next().expect("lease name");
            if method == "DELETE" {
                let parameters: serde_json::Value =
                    serde_json::from_slice(request_body).expect("delete");
                assert_eq!(parameters["preconditions"]["uid"], "lease-original");
                state.leases.remove(name);
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Success"})
            } else if let Some(lease) = state.leases.get(name) {
                lease.clone()
            } else {
                status = 404;
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","code":404})
            }
        } else if method == "POST" && resource_path.ends_with("/configmaps") {
            if state.fail_next_creation_receipt {
                // A persistent non-transient rejection is not masked by the
                // kube client's retry middleware.
                status = 403;
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Forbidden","code":403})
            } else {
                let mut object: serde_json::Value =
                    serde_json::from_slice(request_body).expect("receipt");
                let name = object["metadata"]["name"]
                    .as_str()
                    .expect("name")
                    .to_owned();
                object["metadata"]["uid"] = "receipt-original".into();
                object["metadata"]["resourceVersion"] = "1".into();
                state.archives.insert(name, object.clone());
                object
            }
        } else if resource_path.contains("/configmaps/") {
            if let Some(object) = state
                .archives
                .get(resource_path.rsplit('/').next().expect("name"))
            {
                object.clone()
            } else {
                status = 404;
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","code":404})
            }
        } else if method == "POST" && resource_path.ends_with("/secrets") {
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
        } else if method == "POST" && resource_path.ends_with("/statefulsets") {
            assert!(
                state.sts.is_null(),
                "Missing recovery cannot overwrite another StatefulSet"
            );
            let mut object: serde_json::Value =
                serde_json::from_slice(request_body).expect("create StatefulSet");
            object["metadata"]["uid"] = "sts-created".into();
            object["metadata"]["resourceVersion"] = "10".into();
            object["metadata"]["generation"] = 1.into();
            object["status"] =
                serde_json::json!({"observedGeneration":1,"replicas":1,"readyReplicas":1});
            let mut pod = state.pod_seed.clone();
            pod["metadata"]["uid"] = "pod-created".into();
            pod["metadata"]["ownerReferences"][0]["uid"] = "sts-created".into();
            pod["metadata"]["annotations"] = object["metadata"]["annotations"].clone();
            pod["spec"] = object["spec"]["template"]["spec"].clone();
            state.pod = pod;
            state.sts = object.clone();
            state.puts.push(object.clone());
            lose_response = state.lose_next_create_response;
            state.lose_next_create_response = false;
            object
        } else if method == "GET" && resource_path.ends_with("/persistentvolumeclaims") {
            serde_json::json!({"apiVersion":"v1","kind":"PersistentVolumeClaimList",
                "metadata":{"resourceVersion":"5"},
                "items":if state.pvc.is_null() { vec![] } else { vec![state.pvc.clone()] }})
        } else if resource_path.contains("/persistentvolumeclaims/") {
            state.pvc_reads += 1;
            if state
                .replace_pvc_on_read
                .is_some_and(|read| state.pvc_reads >= read)
            {
                state.pvc["metadata"]["uid"] = "pvc-replacement".into();
            }
            if method == "PATCH" {
                let patch: serde_json::Value =
                    serde_json::from_slice(request_body).expect("PVC patch");
                assert_eq!(patch["metadata"]["uid"], state.pvc["metadata"]["uid"]);
                if let Some(annotations) = patch["metadata"]["annotations"].as_object() {
                    for (key, value) in annotations {
                        state.pvc["metadata"]["annotations"][key] = value.clone();
                    }
                }
            }
            state.pvc.clone()
        } else if method == "PATCH"
            && resource_path.contains("/statefulsets/")
            && serde_json::from_slice::<serde_json::Value>(request_body)
                .expect("patch")
                .get("spec")
                .is_none()
        {
            let patch: serde_json::Value =
                serde_json::from_slice(request_body).expect("metadata fence");
            apply_metadata_fence(&mut state.sts, &patch);
            state.sts.clone()
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
        } else if resource_path.contains("/statefulsets/") && state.sts.is_null() {
            status = 404;
            serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","message":"StatefulSet not found","code":404})
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
            if (state.replace_uid_on_second_sts_read && state.sts_reads >= 2)
                || state
                    .replace_uid_on_sts_read
                    .is_some_and(|read| state.sts_reads >= read)
            {
                object["metadata"]["uid"] = serde_json::json!("foreign-controller");
            }
            object
        } else if resource_path.ends_with("/statefulsets") {
            if watch {
                serde_json::json!({"type":"MODIFIED","object":state.sts})
            } else {
                serde_json::json!({"apiVersion":"apps/v1","kind":"StatefulSetList","metadata":{"resourceVersion":"8"},"items":if state.sts.is_null() { vec![] } else { vec![state.sts.clone()] }})
            }
        } else if resource_path.contains("/pods/") {
            if state.pod.is_null()
                || state.pod["metadata"]["name"]
                    != resource_path.rsplit('/').next().expect("Pod name")
            {
                status = 404;
                serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","message":"Pod not found","code":404})
            } else {
                if method == "PATCH" {
                    let patch = serde_json::from_slice(request_body).expect("Pod metadata fence");
                    apply_metadata_fence(&mut state.pod, &patch);
                }
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
        } else if resource_path.ends_with("/services") {
            serde_json::json!({"apiVersion":"v1","kind":"ServiceList","metadata":{"resourceVersion":"7"},
                "items":[state.service.clone(), state.headless_service.clone()]})
        } else if resource_path.contains("/services/") {
            let service = if resource_path.ends_with("-headless") {
                &mut state.headless_service
            } else {
                &mut state.service
            };
            if method == "PATCH" {
                let patch: serde_json::Value =
                    serde_json::from_slice(request_body).expect("Service patch");
                apply_metadata_fence(service, &patch);
            }
            service.clone()
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
        "namespace":"resume-contract","name":pod_name,"uid":"pod-original","resourceVersion":"9","creationTimestamp":"2026-01-01T00:00:00Z",
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
        "namespace":"resume-contract","name":claim_name,"uid":"pvc-original","resourceVersion":"5","annotations":original.resource_metadata(),
        "labels":{"service_type":ServiceType::UserappBuilder.to_string()}},
        "spec":{"accessModes":["ReadWriteOnce"],"resources":{"requests":{"storage":"1Gi"}}},"status":{"phase":"Bound","capacity":{"storage":"1Gi"}}});
    let mut headless_service = serde_json::to_value(&service).expect("headless Service");
    headless_service["metadata"]["name"] =
        format!("{}-headless", service_name.trim_end_matches("-svc")).into();
    headless_service["metadata"]["uid"] = "headless-original".into();
    headless_service["spec"]["clusterIP"] = "None".into();
    let state = Arc::new(Mutex::new(ApiState {
        sts: object,
        pod_seed: pod.clone(),
        extra_pods: vec![],
        resume_replicas_on_second_sts_read: false,
        fail_pod_list: false,
        replace_uid_on_sts_read: None,
        replace_pvc_on_read: None,
        pvc_reads: 0,
        fail_next_creation_receipt: false,
        pod,
        pvc,
        allow_writes: false,
        lose_next_put_response: false,
        lose_next_create_response: false,
        archives: Default::default(),
        leases: Default::default(),
        puts: vec![],
        patches: vec![],
        service: serde_json::to_value(service).expect("service"),
        headless_service,
        requests: vec![],
        admission_fences: vec![],
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
async fn create_entry_reuses_owned_existing_builder_without_sql_binding() {
    existing_create_case(shared_types::BuilderStartupMode::PreserveBusinessIntent).await;
}

#[tokio::test]
async fn create_entry_management_only_retains_running_business_and_original_request_intent() {
    existing_create_case(shared_types::BuilderStartupMode::ManagementOnly).await;
}

async fn existing_create_case(mode: shared_types::BuilderStartupMode) {
    use container_runtime_api::AgentContainerRuntime as _;
    let (runtime, mut params, state, server) = fixture(Some("release")).await;
    params.resource_binding = None;
    params.storage_size = Some("1Gi".into());
    params.builder_startup_mode = mode;
    state.lock().expect("state").allow_writes = true;
    let before = state.lock().expect("state").sts.clone();
    let before_pvc = state.lock().expect("state").pvc.clone();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        runtime.create_container(params.clone()),
    )
    .await
    .expect("create deadline");
    assert!(
        result.is_ok(),
        "legitimate unbound builder must reuse its actual configuration: {result:?}"
    );
    assert_eq!(result.expect("existing").container_id, "pod-original");
    {
        let actual = state.lock().expect("state");
        assert_eq!(
            without_retirement_fence(actual.sts.clone()),
            without_retirement_fence(before.clone()),
            "only top-level admission RV/nonce may change; retain template, identity, original credential and execution metadata"
        );
        assert_eq!(actual.pvc, before_pvc, "ensure must keep the actual PVC");
        assert!(
            actual.leases.is_empty(),
            "completed request must release its actual operation lease"
        );
        assert!(
            actual.requests.iter().all(|request| {
                request.starts_with("GET ")
                    || request.contains("/leases")
                    || request.contains("/configmaps")
                    || request.contains("/services/")
                    || request.starts_with("PATCH ")
                        && (request.contains("/statefulsets/") || request.contains("/pods/"))
            }),
            "existing compute and data must not be rewritten: {:?}",
            actual.requests
        );
    }
    let receipt = runtime
        .read_builder_creation_receipt(params.execution_context.as_ref().expect("context"))
        .await
        .expect("read original completion")
        .expect("receipt exists");
    assert_eq!(
        receipt.target.context,
        params.execution_context.expect("context")
    );
    assert_eq!(receipt.target.builder_startup_mode, mode);
    assert_eq!(
        receipt.target.workload.expect("workload").uid,
        "sts-original"
    );
    server.abort();
}

#[tokio::test]
async fn create_entry_rejects_foreign_or_replaced_existing_compute_before_data_writes() {
    use container_runtime_api::AgentContainerRuntime as _;
    for mutation in [
        "application",
        "lifecycle",
        "controller-race",
        "volume-race",
        "mount",
    ] {
        let (runtime, mut params, state, server) = fixture(Some("release")).await;
        params.resource_binding = None;
        {
            let mut state = state.lock().expect("state");
            state.allow_writes = true;
            match mutation {
                "application" => {
                    state.sts["metadata"]["annotations"]["rcoder.io/application-id"] =
                        "foreign".into()
                }
                "lifecycle" => {
                    state.sts["metadata"]["annotations"]["rcoder.io/lifecycle-id"] =
                        "foreign".into()
                }
                "controller-race" => state.replace_uid_on_sts_read = Some(3),
                "volume-race" => state.replace_pvc_on_read = Some(2),
                "mount" => {
                    state.sts["spec"]["template"]["spec"]["containers"][0]["volumeMounts"][0]["subPath"] =
                        "other-app".into()
                }
                _ => unreachable!(),
            }
        }
        let result = runtime.create_container(params).await;
        assert!(
            matches!(result, Err(Error::RequestRejected(_))),
            "{mutation}: {result:?}"
        );
        let state = state.lock().expect("state");
        assert!(
            state
                .requests
                .iter()
                .all(|request| request.starts_with("GET ") || request.contains("/leases")),
            "{mutation} must not change compute, data or completion: {:?}",
            state.requests
        );
        assert!(
            state.leases.is_empty(),
            "known rejection must release its own lease"
        );
        server.abort();
    }
}

#[tokio::test]
async fn unbound_reuse_service_commit_recovers_the_original_operation_after_receipt_failure() {
    use container_runtime_api::AgentContainerRuntime as _;
    let (runtime, mut params, state, server) = fixture(Some("release")).await;
    params.resource_binding = None;
    {
        let mut state = state.lock().expect("state");
        state.allow_writes = true;
        state.fail_next_creation_receipt = true;
    }
    let context = params.execution_context.clone().expect("context");
    let result = runtime.create_container(params).await;
    assert!(
        matches!(result, Err(Error::K8sError(_))),
        "receipt dispatch failure remains unconfirmed: {result:?}"
    );
    assert!(!state.lock().expect("state").leases.is_empty());
    let recovered = runtime
        .recover_builder_creation(&context)
        .await
        .expect("original service commitment can reconcile")
        .expect("actual completion evidence");
    assert_eq!(recovered.target.context, context);
    assert_eq!(recovered.container.container_id, "pod-original");
    assert_eq!(
        recovered.target.workload.expect("workload").uid,
        "sts-original"
    );
    assert!(recovered.creation_lease_released);
    let state = state.lock().expect("state");
    assert!(state.leases.is_empty());
    assert!(
        state
            .requests
            .iter()
            .all(|request| request.starts_with("GET ")
                || request.contains("/leases")
                || request.contains("/configmaps")
                || request.contains("/services/")
                || state.admission_fences.contains(request))
    );
    assert_eq!(
        state.admission_fences.len(),
        4,
        "only the original controller, Pod and two Services receive metadata-only fences"
    );
    server.abort();
}

#[tokio::test]
async fn compute_restart_then_cold_unbound_ensure_preserves_updated_compute_and_old_metadata() {
    use container_runtime_api::AgentContainerRuntime as _;
    let (runtime, mut params, state, server) = fixture(Some("release")).await;
    params.resource_binding = None;
    state.lock().expect("state").allow_writes = true;
    let original_pvc = state.lock().expect("state").pvc.clone();
    let mut target = runtime
        .capture_builder_compute_with_binding(
            params.execution_context.as_ref().expect("context"),
            None,
            false,
        )
        .await
        .expect("actual owned controller");
    target.restart_image = Some("paired-runtime:updated".into());
    target.restart_runtime_workspace = Some(shared_types::paths::userapp_dev_workspace("app"));
    let archive = runtime
        .archive_builder_restart(&target)
        .await
        .expect("freeze")
        .expect("archive");
    assert!(
        runtime
            .apply_builder_control(&target, false)
            .await
            .expect("actual stop")
            .is_none()
    );
    let mut stopped = runtime
        .capture_builder_compute_with_binding(
            params.execution_context.as_ref().expect("context"),
            None,
            false,
        )
        .await
        .expect("stopped controller");
    stopped.restart_image = target.restart_image.clone();
    stopped.restart_runtime_workspace = target.restart_runtime_workspace.clone();
    let restarted = runtime
        .start_builder_control_with_restart(&stopped, &archive)
        .await
        .expect("actual frozen restart")
        .expect("ready");
    assert_eq!(restarted.container_id, "pod-updated");
    let updated = state.lock().expect("state").sts.clone();
    let writes_before = state.lock().expect("state").puts.len();
    assert_eq!(
        updated["metadata"]["annotations"]["rcoder.io/request-fingerprint"],
        "a".repeat(64)
    );
    let cold = KubernetesRuntime {
        client: runtime.client.clone(),
        namespace: runtime.namespace.clone(),
        config: runtime.config.clone(),
        pod_cache: Default::default(),
        subvolume_path_cache: Default::default(),
        event_publisher: Default::default(),
        event_counters: Default::default(),
    };
    let context = params.execution_context.as_mut().expect("context");
    context.operation_id = "ordinary-ensure-after-restart".into();
    context.request_fingerprint = "c".repeat(64);
    let ready = cold
        .create_container(params.clone())
        .await
        .expect("cold ordinary ensure reuses actual compute");
    assert_eq!(ready.container_id, "pod-updated");
    assert_eq!(ready.workload_uid.as_deref(), Some("sts-original"));
    {
        let state = state.lock().expect("state");
        assert_eq!(
            without_retirement_fence(state.sts.clone()),
            without_retirement_fence(updated.clone())
        );
        assert_eq!(state.pvc, original_pvc);
        assert_eq!(
            state.puts.len(),
            writes_before,
            "ordinary ensure must not replay configuration update"
        );
        assert!(state.leases.is_empty());
    }
    let completion = cold
        .recover_builder_creation(params.execution_context.as_ref().expect("context"))
        .await
        .expect("exact operation evidence")
        .expect("completion");
    assert_eq!(completion.container.container_id, "pod-updated");
    server.abort();
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

#[tokio::test]
async fn absent_builder_captures_current_retained_pvc_without_requiring_old_workload_uid() {
    let (runtime, params, state, server) = fixture(None).await;
    let original = state.lock().unwrap().pvc.clone();
    {
        let mut state = state.lock().unwrap();
        state.sts = serde_json::Value::Null;
        state.pod = serde_json::Value::Null;
    }
    let target = runtime
        .capture_builder_compute_with_binding(
            params.execution_context.as_ref().unwrap(),
            params.resource_binding.as_ref(),
            false,
        )
        .await
        .unwrap();
    assert!(target.workload.is_none() && target.pod.is_none() && target.resource_binding.is_none());
    let result = runtime.capture_builder_volume_witness(&target).await;
    server.abort();
    let volumes=result.expect("confirmed absent compute must retain a real owned PVC witness, independently of the old registry controller UID");
    assert_eq!(volumes.len(), 1);
    assert_eq!(volumes[0].kind, AppResourceKind::PersistentVolumeClaim);
    assert_eq!(volumes[0].name, original["metadata"]["name"]);
    assert_eq!(volumes[0].uid, "pvc-original");
    let state = state.lock().unwrap();
    assert_eq!(
        state.pvc, original,
        "read-only missing-workload observation cannot mutate storage"
    );
    assert!(
        state
            .requests
            .iter()
            .all(|request| request.starts_with("GET "))
    );
}

#[tokio::test]
async fn absent_builder_volume_witness_rejects_foreign_scope_and_racing_pvc_without_mutation() {
    for changed in [
        "application",
        "lifecycle",
        "family",
        "namespace",
        "name",
        "uid-race",
        "deleting",
        "pod-present",
    ] {
        let (runtime, params, state, server) = fixture(None).await;
        let context = params.execution_context.as_ref().unwrap();
        {
            let mut state = state.lock().unwrap();
            state.sts = serde_json::Value::Null;
            state.pod = serde_json::Value::Null;
        }
        let target = runtime
            .capture_builder_compute_with_binding(context, None, false)
            .await
            .unwrap();
        {
            let mut state = state.lock().unwrap();
            match changed {
                "application" => {
                    state.pvc["metadata"]["annotations"]["rcoder.io/application-id"] =
                        "foreign-app".into()
                }
                "lifecycle" => {
                    state.pvc["metadata"]["annotations"]["rcoder.io/lifecycle-id"] =
                        "foreign-life".into()
                }
                "family" => {
                    state.pvc["metadata"]["labels"]["service_type"] =
                        ServiceType::Userapp.to_string().into()
                }
                "namespace" => state.pvc["metadata"]["namespace"] = "another-namespace".into(),
                "name" => state.pvc["metadata"]["name"] = "another-workspace".into(),
                "uid-race" => state.replace_pvc_on_read = Some(2),
                "deleting" => {
                    state.pvc["metadata"]["deletionTimestamp"] = "2026-10-13T00:00:00Z".into()
                }
                "pod-present" => state.pod = state.pod_seed.clone(),
                _ => unreachable!(),
            }
        }
        let result = runtime.capture_builder_volume_witness(&target).await;
        server.abort();
        assert!(
            result.is_err(),
            "{changed}: absence cannot replace volume/ownership confirmation"
        );
        let state = state.lock().unwrap();
        assert!(
            state
                .requests
                .iter()
                .all(|request| request.starts_with("GET ")),
            "{changed}: must reject before any resource write"
        );
    }
}

#[tokio::test]
async fn absent_builder_uses_original_control_lease_and_retains_pvc_through_real_factory() {
    use container_runtime_api::{AgentContainerRuntime as _, UserAppDeploymentRuntime as _};
    let (runtime, params, state, server) = fixture(None).await;
    let context = params.execution_context.as_ref().unwrap().clone();
    {
        let mut state = state.lock().unwrap();
        state.pvc["spec"]["resources"]["requests"]["storage"] = "100Gi".into();
        state.pvc["status"]["capacity"]["storage"] = "100Gi".into();
    }
    let original_pvc = state.lock().unwrap().pvc.clone();
    {
        let mut state = state.lock().unwrap();
        state.sts = serde_json::Value::Null;
        state.pod = serde_json::Value::Null;
        state.allow_writes = true;
    }
    let mut target = runtime
        .capture_builder_compute_with_binding(&context, None, false)
        .await
        .unwrap();
    target.restart_image = Some("paired-runner:frozen-original".into());
    target.builder_startup_mode = shared_types::BuilderStartupMode::ManagementOnly;
    let volumes = runtime
        .capture_builder_volume_witness(&target)
        .await
        .unwrap();
    let lease = runtime
        .acquire_application_operation_with_context(
            &context.app_id,
            &ServiceType::UserappBuilder,
            Some(&context),
        )
        .await
        .unwrap();
    let receipt = lease.receipt().unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        runtime.ensure_absent_builder_control(&target, &volumes, &receipt),
    )
    .await;
    let first = result
        .expect("fixture deadline")
        .expect("original Missing control must create under its already held lease")
        .expect("running builder");
    assert_eq!(first.container_id, "pod-created");
    assert_eq!(first.workload_uid.as_deref(), Some("sts-created"));
    let before_retry = state.lock().unwrap().requests.len();
    let repeat = runtime
        .ensure_absent_builder_control(&target, &volumes, &receipt)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repeat.container_id, first.container_id);
    assert!(
        runtime
            .builder_compute_write_acknowledged(&target, true)
            .await
            .unwrap()
    );
    assert!(
        runtime
            .reconcile_builder_compute_start(&target)
            .await
            .unwrap()
            .is_some()
    );
    {
        let state = state.lock().unwrap();
        assert_eq!(
            state.pvc["metadata"]["uid"],
            original_pvc["metadata"]["uid"]
        );
        assert_eq!(state.pvc["spec"], original_pvc["spec"]);
        assert_eq!(
            state.pvc["metadata"]["labels"],
            original_pvc["metadata"]["labels"]
        );
        assert_eq!(
            state.sts["spec"]["template"]["spec"]["containers"][0]["image"],
            "paired-runner:frozen-original"
        );
        let env = state.sts["spec"]["template"]["spec"]["containers"][0]["env"]
            .as_array()
            .unwrap();
        assert!(env.iter().any(
            |item| item["name"] == shared_types::APP_CLI_CONTROL_ONLY_ENV
                && item["value"] == "true"
        ));
        assert!(env.iter().any(|item| item["name"]
            == shared_types::APP_CLI_MANAGEMENT_BOOT_ID_ENV
            && item["value"] == context.operation_id));
        assert_eq!(
            state
                .requests
                .iter()
                .filter(|request| request.starts_with("POST ")
                    && request
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .split('?')
                        .next()
                        .unwrap()
                        .ends_with("/leases"))
                .count(),
            1,
            "must not reacquire the existing application lease; actual POSTs: {:?}",
            state
                .requests
                .iter()
                .filter(|request| request.starts_with("POST "))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            state.leases.len(),
            1,
            "the original live lease is still held"
        );
        assert_eq!(
            state
                .puts
                .iter()
                .filter(|object| object["metadata"]["uid"] == "sts-created")
                .count(),
            1,
            "one actual StatefulSet allocation"
        );
        assert_eq!(
            state
                .requests
                .iter()
                .filter(|request| request.starts_with("POST ")
                    && request
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .split('?')
                        .next()
                        .unwrap()
                        .ends_with("/statefulsets"))
                .count(),
            1
        );
        assert!(
            state.requests[before_retry..]
                .iter()
                .all(|request| request.starts_with("GET ")),
            "completed original creation is reconciled read-only"
        );
        assert!(
            !state
                .requests
                .iter()
                .any(|request| request.starts_with("DELETE ")
                    || (request.starts_with("POST ")
                        && request.contains("/persistentvolumeclaims"))),
            "must not delete/recreate PVC or other workloads"
        );
    }
    runtime
        .release_app_operation_receipt(&context, &receipt)
        .await
        .unwrap();
    drop(lease);
    server.abort();
}

#[tokio::test]
async fn completed_builder_creation_is_observed_after_original_lease_expiry_without_any_write() {
    use container_runtime_api::{AgentContainerRuntime as _, UserAppDeploymentRuntime as _};
    tokio::time::timeout(Duration::from_secs(15), async {
        let (runtime, params, state, server) = fixture(None).await;
        let context = params.execution_context.as_ref().unwrap().clone();
        {
            let mut state = state.lock().unwrap();
            state.pvc["spec"]["resources"]["requests"]["storage"] = "100Gi".into();
            state.pvc["status"]["capacity"]["storage"] = "100Gi".into();
            state.sts = serde_json::Value::Null;
            state.pod = serde_json::Value::Null;
            state.allow_writes = true;
        }
        let mut target = runtime
            .capture_builder_compute_with_binding(&context, None, false)
            .await
            .unwrap();
        target.restart_image = Some("paired-runner:frozen-original".into());
        target.builder_startup_mode = shared_types::BuilderStartupMode::ManagementOnly;
        let volumes = runtime
            .capture_builder_volume_witness(&target)
            .await
            .unwrap();
        let lease = runtime
            .acquire_application_operation_with_context(
                &context.app_id,
                &ServiceType::UserappBuilder,
                Some(&context),
            )
            .await
            .unwrap();
        let receipt = lease.receipt().unwrap();
        runtime
            .ensure_absent_builder_control(&target, &volumes, &receipt)
            .await
            .unwrap()
            .unwrap();
        let (original_sts, original_pod, original_service, original_archives) = {
            let mut state = state.lock().unwrap();
            state.allow_writes = false;
            for lease in state.leases.values_mut() {
                lease["spec"]["renewTime"] = (k8s_openapi::jiff::Timestamp::now()
                    - k8s_openapi::jiff::SignedDuration::from_secs(3600))
                .to_string()
                .into();
            }
            (
                state.sts.clone(),
                state.pod.clone(),
                state.service.clone(),
                state.archives.clone(),
            )
        };
        assert!(
            !runtime
                .validate_app_operation_receipt(&context, &receipt)
                .await
                .unwrap()
        );
        assert_eq!(
            runtime
                .observe_app_operation_lease(&context, &receipt)
                .await
                .unwrap(),
            container_runtime_api::AppOperationLeaseObservation::ExpiredOriginalIdentity
        );
        for variant in 0..8 {
            {
                let mut state = state.lock().unwrap();
                state.sts = original_sts.clone();
                state.pod = original_pod.clone();
                state.service = original_service.clone();
                state.archives = original_archives.clone();
                state.requests.clear();
                state.sts_reads = 0;
                state.replace_uid_on_second_sts_read = false;
                match variant {
                    0 => {}
                    1 => {
                        for object in state.archives.values_mut() {
                            if let Some(payload) = object["data"]["receipt"].as_str() {
                                let mut ack: serde_json::Value =
                                    serde_json::from_str(payload).unwrap();
                                ack["lease"]["uid"] = "other-lease-uid".into();
                                object["data"]["receipt"] = ack.to_string().into();
                            }
                        }
                    }
                    2 => {
                        state.archives.clear();
                        if let Some(annotations) =
                            state.service["metadata"]["annotations"].as_object_mut()
                        {
                            annotations.remove("rcoder.io/builder-creation-completion");
                        }
                    }
                    3 => {
                        state.pod["status"]["conditions"] =
                            serde_json::json!([{"type":"Ready","status":"False"}])
                    }
                    4 => state.pod["spec"]["containers"][0]["image"] = "other:image".into(),
                    5 => {
                        let env = state.sts["spec"]["template"]["spec"]["containers"][0]["env"]
                            .as_array_mut()
                            .unwrap();
                        for entry in env {
                            if entry["name"] == shared_types::APP_CLI_CONTROL_ONLY_ENV {
                                entry["value"] = "false".into();
                            }
                        }
                    }
                    6 => state.pod["metadata"]["uid"] = "replacement-pod-uid".into(),
                    7 => state.replace_uid_on_second_sts_read = true,
                    _ => unreachable!(),
                }
            }
            let result = runtime
                .observe_completed_builder_creation(&target, &receipt)
                .await;
            if variant == 0 {
                assert_eq!(result.unwrap().unwrap().container_id, "pod-created");
            } else {
                assert!(
                    !matches!(result, Ok(Some(_))),
                    "variant {variant} unexpectedly confirmed: {result:?}"
                );
            }
            assert!(
                state
                    .lock()
                    .unwrap()
                    .requests
                    .iter()
                    .all(|r| r.starts_with("GET ")),
                "completed result observation must not start, retry, reacquire or mutate"
            );
        }
        server.abort();
        drop(lease);
    })
    .await
    .expect("bounded completed creation HTTP fixture");
}

#[tokio::test]
async fn absent_builder_creation_refuses_replaced_volume_or_foreign_current_operation_before_mutation()
 {
    use container_runtime_api::AgentContainerRuntime as _;
    for changed in ["pvc", "foreign-operation", "lease"] {
        let (runtime, params, state, server) = fixture(None).await;
        let context = params.execution_context.as_ref().unwrap().clone();
        let original_sts = state.lock().unwrap().sts.clone();
        {
            let mut state = state.lock().unwrap();
            state.sts = serde_json::Value::Null;
            state.pod = serde_json::Value::Null;
            state.allow_writes = true;
        }
        let target = runtime
            .capture_builder_compute_with_binding(&context, None, false)
            .await
            .unwrap();
        let volumes = runtime
            .capture_builder_volume_witness(&target)
            .await
            .unwrap();
        let lease = runtime
            .acquire_application_operation_with_context(
                &context.app_id,
                &ServiceType::UserappBuilder,
                Some(&context),
            )
            .await
            .unwrap();
        let mut receipt = lease.receipt().unwrap();
        {
            let mut state = state.lock().unwrap();
            match changed {
                "pvc" => state.pvc["metadata"]["uid"] = "replacement-pvc".into(),
                "foreign-operation" => state.sts = original_sts,
                "lease" => {
                    if let shared_types::UserAppOperationLeaseReceipt::Kubernetes { uid, .. } =
                        &mut receipt
                    {
                        *uid = "foreign-lease".into();
                    }
                }
                _ => unreachable!(),
            }
        }
        let before = state.lock().unwrap().requests.len();
        let result = runtime
            .ensure_absent_builder_control(&target, &volumes, &receipt)
            .await;
        assert!(result.is_err(), "{changed}");
        {
            let state = state.lock().unwrap();
            assert!(
                state.requests[before..]
                    .iter()
                    .all(|request| request.starts_with("GET ")),
                "{changed}: no storage/compute mutation authorized"
            );
        }
        drop(lease);
        server.abort();
    }
}

#[tokio::test]
async fn absent_builder_partial_original_creation_finishes_once_and_ack_is_independent_of_readiness()
 {
    use container_runtime_api::{AgentContainerRuntime as _, UserAppDeploymentRuntime as _};
    let (runtime, params, state, server) = fixture(None).await;
    let context = params.execution_context.as_ref().unwrap().clone();
    {
        let mut state = state.lock().unwrap();
        state.sts = serde_json::Value::Null;
        state.pod = serde_json::Value::Null;
        state.allow_writes = true;
        state.lose_next_create_response = true;
        state.pvc["spec"]["resources"]["requests"]["storage"] = "100Gi".into();
        state.pvc["status"]["capacity"]["storage"] = "100Gi".into();
    }
    let mut target = runtime
        .capture_builder_compute_with_binding(&context, None, false)
        .await
        .unwrap();
    target.builder_startup_mode = shared_types::BuilderStartupMode::ManagementOnly;
    let volumes = runtime
        .capture_builder_volume_witness(&target)
        .await
        .unwrap();
    let lease = runtime
        .prepare_compute_k8s_lease(&context, shared_types::UserAppOperationScope::Dev)
        .await
        .unwrap();
    let receipt = lease.receipt().unwrap();
    assert!(
        runtime
            .prepare_builder_compute_retry(&target)
            .await
            .unwrap()
            .is_none(),
        "unknown actual absence never authorizes blind creation replay"
    );
    let first = tokio::time::timeout(
        Duration::from_secs(15),
        runtime.ensure_absent_builder_control(&target, &volumes, &receipt),
    )
    .await
    .expect("initial budget");
    assert!(
        first.is_err(),
        "a real accepted StatefulSet with lost reply remains unconfirmed"
    );
    assert_eq!(state.lock().unwrap().sts["metadata"]["uid"], "sts-created");
    assert!(
        !runtime
            .builder_compute_write_acknowledged(&target, true)
            .await
            .unwrap(),
        "partial create is not a completed multi-step acknowledgement"
    );
    assert_eq!(
        runtime
            .prepare_builder_compute_retry(&target)
            .await
            .unwrap()
            .unwrap(),
        target,
        "the same recorded partial controller and active original compute holder permit completing only that original intent"
    );
    let resumed = tokio::time::timeout(
        Duration::from_secs(15),
        runtime.ensure_absent_builder_control(&target, &volumes, &receipt),
    )
    .await
    .expect("continuation budget")
    .expect("same original physical operation can complete idempotently")
    .expect("ready");
    assert_eq!(resumed.container_id, "pod-created");
    assert_eq!(state.lock().unwrap().pvc["metadata"]["uid"], "pvc-original");
    assert_eq!(
        state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|request| request.starts_with("POST ")
                && request
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .split('?')
                    .next()
                    .unwrap()
                    .ends_with("/statefulsets"))
            .count(),
        1,
        "owned partial creation must not launch a second controller"
    );
    {
        let mut state = state.lock().unwrap();
        state.pod = serde_json::Value::Null;
        state.sts["spec"]["replicas"] = 0.into();
    }
    assert!(
        runtime
            .builder_compute_write_acknowledged(&target, true)
            .await
            .unwrap(),
        "completed original writes can drain despite stopped/not-ready current compute"
    );
    assert!(
        runtime
            .reconcile_builder_compute_start(&target)
            .await
            .unwrap()
            .is_none(),
        "completed mutation is not proof of present readiness"
    );
    {
        let mut state = state.lock().unwrap();
        state.sts = serde_json::Value::Null;
    }
    assert!(
        runtime
            .builder_compute_write_acknowledged(&target, true)
            .await
            .unwrap(),
        "whole actual absence does not revive a completed old writer"
    );
    {
        let mut state = state.lock().unwrap();
        state.sts = state.puts[0].clone();
        state.sts["metadata"]["uid"] = "another-controller".into();
    }
    assert!(
        runtime
            .builder_compute_write_acknowledged(&target, true)
            .await
            .is_err(),
        "foreign current compute cannot be borrowed as original completion"
    );
    runtime
        .release_app_operation_receipt(&context, &receipt)
        .await
        .unwrap();
    drop(lease);
    server.abort();
}

#[tokio::test]
async fn actual_create_entry_fences_all_retained_services_when_resuming_zero_builder() {
    use container_runtime_api::AgentContainerRuntime as _;
    let (runtime, mut params, state, server) = fixture(Some("release")).await;
    params.resource_binding = None;
    params.builder_startup_mode = shared_types::BuilderStartupMode::PreserveBusinessIntent;
    let original_pvc = {
        let mut current = state.lock().unwrap();
        current.allow_writes = true;
        current.sts["spec"]["replicas"] = 0.into();
        current.pod = serde_json::Value::Null;
        current.pvc.clone()
    };
    let old = runtime
        .capture_agent_stop("app", &ServiceType::UserappBuilder)
        .await
        .unwrap();
    assert!(old.pods.is_empty());
    assert_eq!(
        old.services.len(),
        2,
        "both public and headless Service deletion witnesses exist"
    );
    assert!(
        old.services
            .iter()
            .all(|service| service.resource_version.as_deref() == Some("7"))
    );
    let info = tokio::time::timeout(Duration::from_secs(10), runtime.create_container(params))
        .await
        .expect("original create entry must settle")
        .expect("stopped owned builder resumes");
    assert_eq!(info.workload_uid.as_deref(), Some("sts-original"));
    {
        let current = state.lock().unwrap();
        assert_eq!(
            current.pvc, original_pvc,
            "fencing must preserve actual data PVC"
        );
        assert_eq!(current.sts["spec"]["replicas"], 1);
        for service in [&current.service, &current.headless_service] {
            assert_ne!(service["metadata"]["resourceVersion"], "7");
            assert!(
                service["metadata"]["annotations"]["rcoder.io/agent-admission-fence"]
                    .as_str()
                    .is_some()
            );
        }
        assert!(
            current
                .requests
                .iter()
                .any(|request| request.starts_with("PATCH ") && request.contains("-svc"))
        );
        assert!(
            current
                .requests
                .iter()
                .any(|request| request.starts_with("PATCH ") && request.contains("-headless"))
        );
        assert!(current.leases.is_empty());
    }
    // Model a DELETE submitted from the old Service-only observation, arriving
    // after the actual reuse entry returns. It must not delete either route.
    let mut late_service_delete = old;
    late_service_delete.controller = None;
    late_service_delete.pods.clear();
    let outcome = runtime
        .stop_captured_agent(&late_service_delete)
        .await
        .expect("the captured snapshot contains no compute to stop");
    let container_runtime_api::AgentStopOutcome::ComputeGoneServicesPending { pending } = outcome
    else {
        panic!("both stale Service witnesses must report incomplete cleanup");
    };
    assert_eq!(pending.len(), 2);
    for captured in &late_service_delete.services {
        assert!(pending.iter().any(|failure| {
            failure.name == captured.name
                && failure.uid == captured.uid
                && failure.error.contains("version changed")
        }));
    }
    assert!(
        !state
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|request| request.starts_with("DELETE ") && request.contains("/services/"))
    );
    server.abort();
}
