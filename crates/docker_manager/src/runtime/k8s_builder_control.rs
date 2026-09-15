//! UID/version-fenced builder compute controls; PVC and Service are untouched.
use super::{k8s_pod::K8sPodOps, kubernetes_runtime::KubernetesRuntime};
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use k8s_openapi::api::{apps::v1::StatefulSet, core::v1::Pod};
use kube::{
    Api,
    api::{DeleteParams, Patch, PatchParams, Preconditions},
};
use shared_types::{
    AppResourceIdentity, AppResourceKind, BuilderControlTarget, BuilderPodIdentity,
    ContainerBasicInfo, ServiceType, UserAppExecutionContext,
};
use std::time::Duration;

impl KubernetesRuntime {
    pub(super) async fn resume_bound_builder(
        &self,
        params: &container_runtime_api::ContainerCreateParams,
    ) -> Result<ContainerBasicInfo> {
        let target = async {
            let context = params.execution_context.as_ref().ok_or_else(|| {
                Error::ConfigurationError("Bound builder requires execution context".into())
            })?;
            let binding = params.resource_binding.as_ref().ok_or_else(|| {
                Error::ConfigurationError("Bound builder requires durable resource proof".into())
            })?;
            let target = self
                .capture_builder_compute_with_binding(context, Some(binding), false)
                .await?;
            let workload = target.workload.as_ref().ok_or_else(|| {
                Error::Conflict(
                    "Bound builder disappeared; automatic replacement is forbidden".into(),
                )
            })?;
            let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
            let current = api
                .get(&workload.name)
                .await
                .map_err(|error| api_error("Inspect bound builder configuration", error))?;
            if workload_identity_with_binding(&current, context, Some(binding), false)? != *workload
            {
                return Err(Error::Conflict(
                    "Bound builder changed during configuration validation".into(),
                ));
            }
            let desired =
                self.build_agent_pod_spec(&context.app_id, &ServiceType::UserappBuilder, params)?;
            let actual = current
                .spec
                .as_ref()
                .and_then(|spec| spec.template.spec.as_ref())
                .ok_or_else(|| {
                    Error::ConfigurationError("Bound builder Pod template missing".into())
                })?;
            if !configured_fields_match(
                &serde_json::to_value(desired)
                    .map_err(|error| Error::ConfigurationError(error.to_string()))?,
                &serde_json::to_value(actual)
                    .map_err(|error| Error::ConfigurationError(error.to_string()))?,
            ) {
                return Err(Error::Conflict(
                    "Bound builder Pod configuration changed".into(),
                ));
            }
            Ok::<_, Error>(target)
        }
        .await
        .map_err(|error| rejected_before_write(error.to_string()))?;
        self.apply_builder_compute_mode(&target, true, true)
            .await?
            .ok_or_else(|| Error::Conflict("Bound builder did not return a running Pod".into()))
    }

    pub(super) async fn capture_builder_compute(
        &self,
        context: &UserAppExecutionContext,
    ) -> Result<BuilderControlTarget> {
        self.capture_builder_compute_with_binding(context, None, false)
            .await
    }

    pub(super) async fn capture_builder_compute_with_binding(
        &self,
        context: &UserAppExecutionContext,
        binding: Option<&shared_types::UserAppResourceBinding>,
        adoption: bool,
    ) -> Result<BuilderControlTarget> {
        context
            .validate_identity(&context.app_id)
            .map_err(Error::Conflict)?;
        let name = self.pod_name(&context.app_id, &ServiceType::UserappBuilder)?;
        let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let sts = match api.get(&name).await {
            Ok(sts) => sts,
            Err(kube::Error::Api(error)) if error.code == 404 => {
                return Ok(BuilderControlTarget {
                    resource_binding: None,
                    context: context.clone(),
                    workload: None,
                    pod: None,
                });
            }
            Err(error) => return Err(api_error("Capture builder workload", error)),
        };
        let workload = workload_identity_with_binding(&sts, context, binding, adoption)?;
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let pod_name = self.agent_pod_name(&context.app_id, &ServiceType::UserappBuilder)?;
        let pod = match pods.get(&pod_name).await {
            Ok(pod) => Some(pod_identity(&pod, &workload)?),
            Err(kube::Error::Api(error)) if error.code == 404 => None,
            Err(error) => return Err(api_error("Capture builder pod", error)),
        };
        Ok(BuilderControlTarget {
            resource_binding: binding.cloned(),
            context: context.clone(),
            workload: Some(workload),
            pod,
        })
    }

    pub(super) async fn apply_builder_compute(
        &self,
        target: &BuilderControlTarget,
        restart: bool,
    ) -> Result<Option<ContainerBasicInfo>> {
        self.apply_builder_compute_mode(target, restart, false)
            .await
    }

    async fn apply_builder_compute_mode(
        &self,
        target: &BuilderControlTarget,
        restart: bool,
        only_start: bool,
    ) -> Result<Option<ContainerBasicInfo>> {
        target.validate().map_err(rejected_before_write)?;
        let Some(workload) = &target.workload else {
            return Ok(None);
        };
        if workload.kind != AppResourceKind::StatefulSet {
            return Err(rejected_before_write(
                "Non-Kubernetes builder control target".into(),
            ));
        }
        let sts_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &self.namespace);
        let current = sts_api
            .get(&workload.name)
            .await
            .map_err(|error| rejected_before_write(format!("Verify builder workload: {error}")))?;
        if workload_identity_with_binding(
            &current,
            &target.context,
            target.resource_binding.as_ref(),
            false,
        )
        .map_err(|error| rejected_before_write(error.to_string()))?
            != *workload
        {
            return Err(rejected_before_write(
                "Builder workload changed after capture".into(),
            ));
        }
        if restart && only_start {
            let replicas = current
                .spec
                .as_ref()
                .and_then(|spec| spec.replicas)
                .unwrap_or(1);
            if !matches!(replicas, 0 | 1) {
                return Err(rejected_before_write(
                    "Bound builder has unexpected replicas".into(),
                ));
            }
            if replicas == 0 {
                sts_api.patch(&workload.name, &PatchParams::default(),
                    &Patch::Merge(serde_json::json!({
                        "metadata": {"uid": workload.uid, "resourceVersion": workload.resource_version},
                        "spec": {"replicas": 1}
                    }))).await.map_err(|error| super::builder_completion::k8s_error(
                        format!("Wake captured builder workload: {error}"), error))?;
            }
        } else if restart {
            let pod = target.pod.as_ref().ok_or_else(|| {
                rejected_before_write("Builder restart requires a captured pod".into())
            })?;
            let actual = pods
                .get(&pod.name)
                .await
                .map_err(|error| rejected_before_write(format!("Verify restart pod: {error}")))?;
            if pod_identity(&actual, workload)
                .map_err(|error| rejected_before_write(error.to_string()))?
                != *pod
            {
                return Err(rejected_before_write(
                    "Builder pod changed after capture".into(),
                ));
            }
            pods.delete(&pod.name, &pod_delete_params(pod))
                .await
                .map_err(|error| {
                    super::builder_completion::k8s_error(
                        format!("Restart captured builder pod: {error}"),
                        error,
                    )
                })?;
        } else {
            sts_api
                .patch(
                    &workload.name,
                    &PatchParams::default(),
                    &Patch::Merge(stop_patch(workload)),
                )
                .await
                .map_err(|error| {
                    super::builder_completion::k8s_error(
                        format!("Stop captured builder workload: {error}"),
                        error,
                    )
                })?;
        }
        // Only observation is time bounded. A timeout never authorizes lease
        // release; the caller records an uncertain operation for reconciliation.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        tokio::time::timeout_at(deadline, async {
            loop {
                let current = sts_api
                    .get(&workload.name)
                    .await
                    .map_err(|error| api_error("Observe builder workload", error))?;
                let identity = workload_identity_with_binding(
                    &current,
                    &target.context,
                    target.resource_binding.as_ref(),
                    false,
                )?;
                if identity.uid != workload.uid {
                    return Err(Error::Conflict(
                        "Builder workload replaced during control".into(),
                    ));
                }
                if only_start
                    && current
                        .spec
                        .as_ref()
                        .and_then(|spec| spec.replicas)
                        .unwrap_or(1)
                        != 1
                {
                    return Err(Error::Conflict(
                        "Builder replicas changed while waking".into(),
                    ));
                }
                if !restart && current.spec.as_ref().and_then(|spec| spec.replicas) != Some(0) {
                    return Err(Error::Conflict(
                        "Builder replicas changed while stopping".into(),
                    ));
                }
                let name =
                    self.agent_pod_name(&target.context.app_id, &ServiceType::UserappBuilder)?;
                match pods.get(&name).await {
                    Err(kube::Error::Api(error)) if error.code == 404 && !restart => {
                        return Ok(None);
                    }
                    Err(kube::Error::Api(error)) if error.code == 404 => {}
                    Err(error) => return Err(api_error("Observe controlled builder pod", error)),
                    Ok(pod) => {
                        let observed = pod_identity(&pod, workload)?;
                        if restart {
                            if !only_start
                                && target
                                    .pod
                                    .as_ref()
                                    .is_some_and(|old| old.uid == observed.uid)
                            {
                                tokio::time::sleep(Duration::from_millis(200)).await;
                                continue;
                            }
                            if pod.metadata.deletion_timestamp.is_none()
                                && pod
                                    .status
                                    .as_ref()
                                    .and_then(|status| status.conditions.as_ref())
                                    .is_some_and(|conditions| {
                                        conditions.iter().any(|condition| {
                                            condition.type_ == "Ready" && condition.status == "True"
                                        })
                                    })
                            {
                                let endpoint =
                                    super::k8s_builder_deletion::workspace_endpoint_from_bound_pod(
                                        &pod,
                                        workload,
                                        &target.context,
                                        target.resource_binding.as_ref(),
                                    )?;
                                let created_at = pod
                                    .metadata
                                    .creation_timestamp
                                    .as_ref()
                                    .ok_or_else(|| {
                                        Error::ConfigurationError(
                                            "Builder pod creation time is missing".into(),
                                        )
                                    })?
                                    .0;
                                let created_at = chrono::DateTime::from_timestamp(
                                    created_at.as_second(),
                                    created_at.subsec_nanosecond() as u32,
                                )
                                .ok_or_else(|| {
                                    Error::ConfigurationError(
                                        "Builder pod creation time is out of range".into(),
                                    )
                                })?;
                                return Ok(Some(ContainerBasicInfo {
                                    container_id: endpoint.container_id,
                                    container_name: name,
                                    container_ip: endpoint.address.to_string(),
                                    internal_port: shared_types::GRPC_DEFAULT_PORT,
                                    external_port: 0,
                                    project_id: target.context.app_id.clone(),
                                    status: "Running".into(),
                                    created_at,
                                    service_url: format!(
                                        "http://{}:{}",
                                        endpoint.address,
                                        shared_types::GRPC_DEFAULT_PORT
                                    ),
                                }));
                            }
                        } else if target
                            .pod
                            .as_ref()
                            .is_some_and(|old| old.uid != observed.uid)
                        {
                            return Err(Error::Conflict(
                                "A replacement builder pod appeared while stopping".into(),
                            ));
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .map_err(|_| {
            Error::Timeout("Builder compute confirmation timed out; reconciliation required".into())
        })?
    }
}

/// Kubernetes may populate omitted defaults, but every explicitly requested
/// field and ordered list entry must match. Sidecars/config changes are rejected.
fn configured_fields_match(desired: &serde_json::Value, actual: &serde_json::Value) -> bool {
    match (desired, actual) {
        (serde_json::Value::Object(expected), serde_json::Value::Object(observed)) => {
            expected.iter().all(|(key, value)| {
                observed
                    .get(key)
                    .is_some_and(|actual| configured_fields_match(value, actual))
            })
        }
        (serde_json::Value::Array(expected), serde_json::Value::Array(observed)) => {
            expected.len() == observed.len()
                && expected
                    .iter()
                    .zip(observed)
                    .all(|(value, actual)| configured_fields_match(value, actual))
        }
        _ => desired == actual,
    }
}

fn rejected_before_write(message: String) -> Error {
    Error::RequestRejected(shared_types::RuntimeRequestRejection {
        status: 409,
        message,
    })
}

fn stop_patch(workload: &AppResourceIdentity) -> serde_json::Value {
    serde_json::json!({"metadata":{"uid":workload.uid,"resourceVersion":workload.resource_version},"spec":{"replicas":0}})
}
fn pod_delete_params(pod: &BuilderPodIdentity) -> DeleteParams {
    DeleteParams {
        preconditions: Some(Preconditions {
            uid: Some(pod.uid.clone()),
            resource_version: Some(pod.resource_version.clone()),
        }),
        ..Default::default()
    }
}
fn required(value: Option<&str>, field: &str) -> Result<String> {
    value
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::ConfigurationError(format!("Builder {field} is missing")))
}
#[cfg(test)]
fn workload_identity(
    sts: &StatefulSet,
    context: &UserAppExecutionContext,
) -> Result<AppResourceIdentity> {
    workload_identity_with_binding(sts, context, None, false)
}

fn workload_identity_with_binding(
    sts: &StatefulSet,
    context: &UserAppExecutionContext,
    binding: Option<&shared_types::UserAppResourceBinding>,
    adoption: bool,
) -> Result<AppResourceIdentity> {
    if sts.metadata.deletion_timestamp.is_some() {
        return Err(Error::Conflict("Builder workload is deleting".into()));
    }
    let labels = sts
        .metadata
        .labels
        .as_ref()
        .ok_or_else(|| Error::Conflict("Builder workload labels missing".into()))?;
    if labels.get("rcoder.io/service-type").map(String::as_str)
        != Some(ServiceType::UserappBuilder.to_string().as_str())
        || labels.get("rcoder.io/identifier").map(String::as_str) != Some(context.app_id.as_str())
    {
        return Err(Error::Conflict(
            "Builder workload ownership mismatch".into(),
        ));
    }
    let metadata = sts.metadata.annotations.clone().unwrap_or_default();
    let uid = required(sts.metadata.uid.as_deref(), "workload UID")?;
    if !shared_types::builder_identity_is_bound(context, &metadata, &uid, binding)
        .map_err(Error::Conflict)?
        && !adoption
    {
        return Err(Error::Conflict(
            "Builder requires explicit physical resource adoption".into(),
        ));
    }
    Ok(AppResourceIdentity {
        kind: AppResourceKind::StatefulSet,
        name: required(sts.metadata.name.as_deref(), "workload name")?,
        uid: required(sts.metadata.uid.as_deref(), "workload UID")?,
        resource_version: Some(required(
            sts.metadata.resource_version.as_deref(),
            "workload version",
        )?),
    })
}
fn pod_identity(pod: &Pod, workload: &AppResourceIdentity) -> Result<BuilderPodIdentity> {
    if !pod
        .metadata
        .owner_references
        .as_ref()
        .is_some_and(|owners| {
            owners.iter().any(|owner| {
                owner.controller == Some(true)
                    && owner.api_version == "apps/v1"
                    && owner.kind == "StatefulSet"
                    && owner.name == workload.name
                    && owner.uid == workload.uid
            })
        })
    {
        return Err(Error::Conflict(
            "Builder pod belongs to another workload".into(),
        ));
    }
    Ok(BuilderPodIdentity {
        name: required(pod.metadata.name.as_deref(), "pod name")?,
        uid: required(pod.metadata.uid.as_deref(), "pod UID")?,
        resource_version: required(pod.metadata.resource_version.as_deref(), "pod version")?,
    })
}
fn api_error(context: &str, error: kube::Error) -> Error {
    if matches!(&error, kube::Error::Api(response) if response.code == 409) {
        Error::Conflict(format!("{context}: {error}"))
    } else {
        Error::K8sError(format!("{context}: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    async fn stop_api_contract(reject_patch: bool, restart: bool, wake: bool, replaced: bool) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
        let mut ready_pod = serde_json::json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":"rcoder-app-builder-app-0","uid":"ready-pod","resourceVersion":"10","creationTimestamp":"2026-01-01T00:00:00Z","labels":{"rcoder.io/service-type":ServiceType::UserappBuilder.to_string(),"rcoder.io/identifier":"app"},"annotations":context.resource_metadata(),"ownerReferences":[{"apiVersion":"apps/v1","kind":"StatefulSet","name":"builder","uid":"sts-original","controller":true}]},"status":{"phase":"Running","podIP":"10.0.0.9","conditions":[{"type":"Ready","status":"True"}]}});
        if wake {
            object["metadata"]["annotations"] = serde_json::json!({});
            object["spec"]["template"]["spec"]["containers"] =
                serde_json::json!([{"name":"agent","env":[{"name":"USER_ID","value":"owner"}]}]);
            ready_pod["metadata"]["annotations"] = serde_json::json!({});
            ready_pod["spec"] = serde_json::json!({"containers":[{"name":"agent","env":[{"name":"USER_ID","value":"owner"}]}]});
        }
        let workload = workload_identity_with_binding(
            &serde_json::from_value(object.clone()).expect("workload"),
            &context,
            None,
            wake,
        )
        .expect("identity");
        if replaced {
            object["metadata"]["uid"] = serde_json::json!("replacement-sts");
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            for index in 0..if replaced {
                1
            } else if restart {
                3
            } else if reject_patch {
                2
            } else {
                4
            } {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut bytes = Vec::new();
                let mut buffer = [0u8; 2048];
                let (headers, body) = loop {
                    let n = stream.read(&mut buffer).await.expect("read");
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..end]).to_string();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().expect("length"))
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break (headers, bytes[end + 4..end + 4 + length].to_vec());
                        }
                    }
                };
                let first = headers.lines().next().expect("request");
                assert!(!first.contains("persistentvolumeclaims") && !first.contains("services"));
                if restart && index == 2 {
                    let parts: Vec<_> = first.split_whitespace().collect();
                    assert_eq!(parts.len(), 3, "unexpected request: {first}");
                    assert_eq!(parts[0], "DELETE");
                    assert_eq!(
                        parts[1].strip_suffix('?').unwrap_or(parts[1]),
                        "/api/v1/namespaces/review-test/pods/builder-0"
                    );
                    assert_eq!(parts[2], "HTTP/1.1");
                    let params: serde_json::Value =
                        serde_json::from_slice(&body).expect("delete body");
                    assert_eq!(params["preconditions"]["uid"], "pod-original");
                    assert_eq!(params["preconditions"]["resourceVersion"], "9");
                } else if index == 1 && !restart {
                    let parts: Vec<_> = first.split_whitespace().collect();
                    assert_eq!(parts.len(), 3, "unexpected request: {first}");
                    assert_eq!(parts[0], "PATCH");
                    // kube includes an empty query delimiter with default PatchParams.
                    assert_eq!(
                        parts[1].strip_suffix('?').unwrap_or(parts[1]),
                        "/apis/apps/v1/namespaces/review-test/statefulsets/builder"
                    );
                    assert_eq!(parts[2], "HTTP/1.1");
                    let patch: serde_json::Value = serde_json::from_slice(&body).expect("patch");
                    assert_eq!(patch["metadata"]["uid"], "sts-original");
                    assert_eq!(patch["metadata"]["resourceVersion"], "8");
                    assert_eq!(patch["spec"]["replicas"], if wake { 1 } else { 0 });
                } else {
                    assert!(first.starts_with("GET "));
                }
                let (code, response) = if restart && index == 1 {
                    (
                        200,
                        serde_json::json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":"builder-0","uid":"pod-original","resourceVersion":"9","ownerReferences":[{"apiVersion":"apps/v1","kind":"StatefulSet","name":"builder","uid":"sts-original","controller":true}]}}),
                    )
                } else if (restart && index == 2) || (!restart && index == 1 && reject_patch) {
                    (
                        403,
                        serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Forbidden","message":"Patch denied","code":403}),
                    )
                } else if index == 3 && wake {
                    assert!(first.contains("/pods/"));
                    (200, ready_pod.clone())
                } else if index == 3 {
                    assert!(first.contains("/pods/"));
                    (
                        404,
                        serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","message":"Pod absent","code":404}),
                    )
                } else {
                    let mut observed = object.clone();
                    if index > 0 {
                        observed["spec"]["replicas"] = serde_json::json!(if wake { 1 } else { 0 });
                    }
                    (200, observed)
                };
                let body = response.to_string();
                stream.write_all(format!("HTTP/1.1 {code} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.expect("respond");
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
            },
            pod_cache: Default::default(),
            subvolume_path_cache: Default::default(),
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
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            let outcome = runtime
                .apply_builder_compute_mode(&target, restart || wake, wake)
                .await;
            if reject_patch || replaced {
                assert!(matches!(outcome, Err(Error::RequestRejected(_))));
            } else if wake {
                assert_eq!(
                    outcome.expect("wake").expect("ready").container_id,
                    "ready-pod"
                );
            } else {
                assert!(outcome.expect("stop").is_none());
            }
            server.await.expect("server");
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
}
