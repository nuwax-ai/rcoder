//! UserApp builder receipts bind StatefulSet, Services and PVC to their original UID/version.
use super::{
    k8s_pod::K8sPodOps, k8s_pvc::K8sPvcOps, k8s_service::K8sServiceOps,
    kubernetes_runtime::KubernetesRuntime,
};
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use kube::{
    Api,
    api::{DeleteParams, Patch, PatchParams, Preconditions, PropagationPolicy},
    core::{ApiResource, DynamicObject, GroupVersionKind},
};
use shared_types::{
    AppResourceIdentity, AppResourceKind as Kind, BuilderDeletionSnapshot, ServiceType,
};

impl KubernetesRuntime {
    pub(super) async fn captured_builder_workspace(
        &self,
        snapshot: &BuilderDeletionSnapshot,
        context: &shared_types::UserAppExecutionContext,
    ) -> Result<shared_types::UserAppBuilderWorkspaceEndpoint> {
        context
            .validate_identity(&snapshot.app_id)
            .map_err(Error::Conflict)?;
        let mut workloads = snapshot
            .resources
            .iter()
            .filter(|resource| resource.kind == Kind::StatefulSet);
        let workload = workloads
            .next()
            .ok_or_else(|| Error::Conflict("Captured builder StatefulSet is missing".into()))?;
        if workloads.next().is_some() || workload.uid.is_empty() {
            return Err(Error::Conflict(
                "Captured builder StatefulSet is ambiguous".into(),
            ));
        }
        let actual = self
            .capture_builder_compute_with_binding(
                context,
                snapshot.resource_binding.as_ref(),
                false,
            )
            .await?;
        if actual
            .workload
            .as_ref()
            .map(|resource| resource.uid.as_str())
            != Some(workload.uid.as_str())
        {
            return Err(Error::Conflict("Captured builder workload changed".into()));
        }
        let pod_name = self.agent_pod_name(&snapshot.app_id, &ServiceType::UserappBuilder)?;
        let pod = self
            .pods()
            .get(&pod_name)
            .await
            .map_err(|error| map_error("inspect captured builder pod", error))?;
        workspace_endpoint_from_bound_pod(
            &pod,
            workload,
            context,
            snapshot.resource_binding.as_ref(),
        )
    }

    fn builder_api(&self, kind: Kind) -> Result<Api<DynamicObject>> {
        let (group, name) = match kind {
            Kind::StatefulSet => ("apps", "StatefulSet"),
            Kind::Service => ("", "Service"),
            Kind::PersistentVolumeClaim => ("", "PersistentVolumeClaim"),
            _ => {
                return Err(Error::ConfigurationError(
                    "invalid builder resource kind".into(),
                ));
            }
        };
        let resource = ApiResource::from_gvk(&GroupVersionKind::gvk(group, "v1", name));
        Ok(Api::namespaced_with(
            self.client.clone(),
            &self.namespace,
            &resource,
        ))
    }

    /// 按 app 维度枚举全部 builder 实例 identifier（协作模型多实例）：
    /// STS 与 PVC 双源（`rcoder.io/app-id` label——STS 覆盖在跑实例、PVC 覆盖
    /// 孤儿卷），读 `rcoder.io/identifier` 收集复合键。注意两族 service_type
    /// 标签键不同（STS `rcoder.io/service-type`、PVC `service_type`）。
    pub(super) async fn find_builder_instances(&self, app_id: &str) -> Result<Vec<String>> {
        use kube::api::ListParams;
        let selectors = [
            format!(
                "rcoder.io/app-id={app_id},rcoder.io/service-type={}",
                ServiceType::UserappBuilder
            ),
            format!(
                "rcoder.io/app-id={app_id},service_type={}",
                ServiceType::UserappBuilder
            ),
        ];
        let mut identifiers = std::collections::BTreeSet::new();
        for (kind, selector) in [
            (Kind::StatefulSet, &selectors[0]),
            (Kind::PersistentVolumeClaim, &selectors[1]),
        ] {
            let lp = ListParams::default().labels(selector);
            let items = self
                .builder_api(kind)?
                .list(&lp)
                .await
                .map_err(|error| map_error("list builder instances", error))?;
            for object in items {
                if let Some(identifier) = object
                    .metadata
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get("rcoder.io/identifier"))
                    .filter(|id| shared_types::parse_builder_instance_id(id).is_some())
                {
                    identifiers.insert(identifier.clone());
                }
            }
        }
        Ok(identifiers.into_iter().collect())
    }

    pub(super) async fn capture_builder(&self, app_id: &str) -> Result<BuilderDeletionSnapshot> {
        let family = ServiceType::UserappBuilder;
        let mut snapshot = BuilderDeletionSnapshot {
            resource_binding: None,
            app_id: app_id.into(),
            operation_id: uuid::Uuid::new_v4().to_string(),
            resources: vec![],
            docker_bind_cleanup: false,
        };
        for (kind, name) in [
            (Kind::StatefulSet, self.pod_name(app_id, &family)?),
            (Kind::Service, self.agent_service_name(app_id, &family)?),
            (
                Kind::Service,
                self.agent_headless_svc_name(app_id, &family)?,
            ),
            (
                Kind::PersistentVolumeClaim,
                self.workspace_pvc_name(app_id, &family)?,
            ),
        ] {
            match self.builder_api(kind)?.get(&name).await {
                Ok(object) => {
                    validate_owner(kind, &object, app_id)?;
                    snapshot.resources.push(identity(kind, object)?);
                }
                Err(kube::Error::Api(error)) if error.code == 404 => {}
                Err(error) => return Err(map_error("capture builder", error)),
            }
        }
        Ok(snapshot)
    }

    pub(super) async fn claim_builder_storage_with_context(
        &self,
        app_id: &str,
        context: Option<&shared_types::UserAppExecutionContext>,
    ) -> Result<()> {
        if let Some(context) = context {
            context
                .validate_identity(app_id)
                .map_err(Error::ConfigurationError)?;
        }
        let operation = context
            .map(|context| context.operation_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        // One budget and one operation identity cover all attempts. A timed-out
        // PATCH is an unknown write: callers must retain the recovery fence.
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.claim_builder_storage_inner(app_id, &operation, context),
        )
        .await
        .map_err(|_| {
            Error::Timeout(
                "builder storage claim exceeded its deadline; outcome requires verification".into(),
            )
        })?
    }

    async fn claim_builder_storage_inner(
        &self,
        app_id: &str,
        operation: &str,
        context: Option<&shared_types::UserAppExecutionContext>,
    ) -> Result<()> {
        let name = self.workspace_pvc_name(app_id, &ServiceType::UserappBuilder)?;
        let api = self.builder_api(Kind::PersistentVolumeClaim)?;
        let mut original = None;
        for attempt in 0..4 {
            let object = api.get(&name).await.map_err(|error| {
                super::builder_completion::k8s_error(
                    format!("read builder storage claim: {error}"),
                    error,
                )
            })?;
            validate_owner(Kind::PersistentVolumeClaim, &object, app_id)?;
            if let Some(context) = context {
                let annotations = object.metadata.annotations.as_ref();
                for (key, expected) in [
                    ("rcoder.io/lifecycle-id", &context.lifecycle_id),
                    ("rcoder.io/owner-id", &context.user_id),
                ] {
                    if let Some(actual) = annotations.and_then(|values| values.get(key))
                        && actual != expected
                    {
                        return Err(Error::Conflict(
                            "Builder storage lifecycle ownership changed".into(),
                        ));
                    }
                }
            }
            if object.metadata.deletion_timestamp.is_some() {
                return Err(Error::Conflict("builder storage is terminating".into()));
            }
            // Conservatively reject label changes as well as replacement. Once
            // lifecycle labels are installed this also fences a new lifetime.
            let owner = object.metadata.labels.clone();
            let receipt = identity(Kind::PersistentVolumeClaim, object)?;
            match &original {
                Some((uid, labels)) if uid != &receipt.uid || labels != &owner => {
                    return Err(Error::Conflict(
                        "builder storage identity changed while claiming".into(),
                    ));
                }
                None => original = Some((receipt.uid.clone(), owner)),
                _ => {}
            }
            let mut annotations = serde_json::Map::new();
            annotations.insert("rcoder.io/storage-use-operation".into(), operation.into());
            if let Some(context) = context {
                annotations.insert(
                    "rcoder.io/lifecycle-id".into(),
                    context.lifecycle_id.clone().into(),
                );
                annotations.insert("rcoder.io/owner-id".into(), context.user_id.clone().into());
            }
            let patch = serde_json::json!({"metadata":{"uid":receipt.uid,"resourceVersion":receipt.resource_version,"annotations":annotations}});
            match api
                .patch(&name, &PatchParams::default(), &Patch::Merge(patch))
                .await
            {
                Ok(_) => return Ok(()),
                Err(kube::Error::Api(response)) if response.code == 409 && attempt < 3 => {
                    let jitter = operation
                        .bytes()
                        .fold(attempt as u64, |sum, byte| sum + u64::from(byte))
                        % 25;
                    tokio::time::sleep(std::time::Duration::from_millis(
                        25 * (1 << attempt) + jitter,
                    ))
                    .await;
                }
                Err(error) => {
                    return Err(super::builder_completion::k8s_error(
                        format!("claim builder storage: {error}"),
                        error,
                    ));
                }
            }
        }
        Err(Error::Conflict(
            "builder storage claim retry budget exhausted".into(),
        ))
    }

    pub(super) async fn delete_captured_builder(
        &self,
        snapshot: &BuilderDeletionSnapshot,
    ) -> Result<()> {
        if snapshot.docker_bind_cleanup {
            return Err(Error::ConfigurationError(
                "Docker builder receipt passed to Kubernetes".into(),
            ));
        }
        let current = self.capture_builder(&snapshot.app_id).await?;
        if current
            .resources
            .iter()
            .any(|resource| !snapshot.resources.contains(resource))
        {
            return Err(Error::Conflict(
                "builder resources changed after capture".into(),
            ));
        }
        for receipt in &snapshot.resources {
            let api = self.builder_api(receipt.kind)?;
            let params = deletion_params(receipt)?;
            match api.delete(&receipt.name, &params).await {
                Ok(_) => {}
                Err(kube::Error::Api(error)) if error.code == 404 => {}
                Err(error) => return Err(map_error("delete captured builder", error)),
            }
            // Foreground STS deletion must finish before PVC removal; a replacement
            // with the same name is a conflict, never another deletion target.
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                match api.get(&receipt.name).await {
                    Err(kube::Error::Api(error)) if error.code == 404 => break,
                    Ok(object) if object.metadata.uid.as_deref() != Some(receipt.uid.as_str()) => {
                        return Err(Error::Conflict(
                            "builder resource replaced during deletion".into(),
                        ));
                    }
                    Ok(_) => {}
                    Err(error) => return Err(map_error("observe builder deletion", error)),
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(Error::Timeout(
                        "builder resource remains terminating".into(),
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
        if !self
            .capture_builder(&snapshot.app_id)
            .await?
            .resources
            .is_empty()
        {
            return Err(Error::Conflict(
                "builder replaced before cleanup completion".into(),
            ));
        }
        {
            let mut cache = self.pod_cache.write().await;
            if cache
                .get(&snapshot.app_id)
                .is_some_and(|cached| cached.service_type == ServiceType::UserappBuilder)
            {
                cache.remove(&snapshot.app_id);
            }
        }
        let pvc_name = self.workspace_pvc_name(&snapshot.app_id, &ServiceType::UserappBuilder)?;
        self.subvolume_path_cache.write().await.remove(&pvc_name);
        Ok(())
    }
}

#[cfg(test)]
pub(super) fn workspace_endpoint_from_pod(
    pod: &k8s_openapi::api::core::v1::Pod,
    workload: &AppResourceIdentity,
    context: &shared_types::UserAppExecutionContext,
) -> Result<shared_types::UserAppBuilderWorkspaceEndpoint> {
    workspace_endpoint_from_bound_pod(pod, workload, context, None)
}

pub(super) fn workspace_endpoint_from_bound_pod(
    pod: &k8s_openapi::api::core::v1::Pod,
    workload: &AppResourceIdentity,
    context: &shared_types::UserAppExecutionContext,
    binding: Option<&shared_types::UserAppResourceBinding>,
) -> Result<shared_types::UserAppBuilderWorkspaceEndpoint> {
    if pod.metadata.deletion_timestamp.is_some()
        || !pod
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|owners| {
                owners.iter().any(|owner| {
                    owner.controller == Some(true)
                        && owner.kind == "StatefulSet"
                        && owner.api_version == "apps/v1"
                        && owner.uid == workload.uid
                        && owner.name == workload.name
                })
            })
    {
        return Err(Error::Conflict(
            "Builder pod does not belong to the captured workload".into(),
        ));
    }
    let labels = pod
        .metadata
        .labels
        .as_ref()
        .ok_or_else(|| Error::Conflict("Builder pod family labels are missing".into()))?;
    let (family, slots) = super::k8s_service::container_identity_from_labels(labels);
    if family != Some(ServiceType::UserappBuilder)
        || family.as_ref().and_then(|family| {
            family
                .container_identifier(
                    slots.pod_id.as_deref(),
                    slots.user_id.as_deref(),
                    slots.project_id.as_deref().or(slots.app_id.as_deref()),
                )
                .ok()
        }) != Some(context.app_id.as_str())
    {
        return Err(Error::Conflict(
            "Builder pod family or application mismatch".into(),
        ));
    }
    let annotations = pod.metadata.annotations.clone().unwrap_or_default();
    let owner = annotations
        .get("rcoder.io/owner-id")
        .map(String::as_str)
        .or_else(|| {
            pod.spec
                .as_ref()
                .and_then(|spec| {
                    spec.containers
                        .iter()
                        .find(|container| container.name == "agent")
                })
                .and_then(|container| container.env.as_ref())
                .and_then(|env| {
                    env.iter()
                        .find(|entry| entry.name == "USER_ID" && entry.value_from.is_none())
                })
                .and_then(|entry| entry.value.as_deref())
        });
    if !shared_types::builder_identity_is_bound(
        context,
        &annotations,
        &workload.uid,
        owner,
        binding,
    )
    .map_err(Error::Conflict)?
    {
        return Err(Error::Conflict(
            "Builder pod requires explicit physical resource adoption".into(),
        ));
    }
    let status = pod
        .status
        .as_ref()
        .ok_or_else(|| Error::Conflict("Builder pod status is missing".into()))?;
    if status.phase.as_deref() != Some("Running")
        || !status.conditions.as_ref().is_some_and(|conditions| {
            conditions
                .iter()
                .any(|condition| condition.type_ == "Ready" && condition.status == "True")
        })
    {
        return Err(Error::Conflict("Captured builder pod is not ready".into()));
    }
    let container_id = pod
        .metadata
        .uid
        .as_ref()
        .filter(|uid| !uid.is_empty())
        .ok_or_else(|| Error::Conflict("Builder pod UID is missing".into()))?
        .clone();
    let address = status
        .pod_ip
        .as_deref()
        .ok_or_else(|| Error::Conflict("Builder pod IP is missing".into()))?
        .parse::<std::net::IpAddr>()
        .map_err(|error| Error::ConfigurationError(format!("Invalid builder pod IP: {error}")))?;
    if address.is_unspecified() {
        return Err(Error::ConfigurationError(
            "Builder pod IP is unspecified".into(),
        ));
    }
    Ok(shared_types::UserAppBuilderWorkspaceEndpoint {
        container_id,
        address,
    })
}

fn validate_owner(kind: Kind, object: &DynamicObject, app_id: &str) -> Result<()> {
    let key = if kind == Kind::PersistentVolumeClaim {
        "service_type"
    } else {
        "rcoder.io/service-type"
    };
    let labels = object
        .metadata
        .labels
        .as_ref()
        .ok_or_else(|| Error::Conflict("builder ownership labels missing".into()))?;
    if labels.get(key).map(String::as_str) != Some(ServiceType::UserappBuilder.to_string().as_str())
    {
        return Err(Error::Conflict("resource is not a UserApp builder".into()));
    }
    if kind != Kind::PersistentVolumeClaim
        && labels.get("rcoder.io/identifier").map(String::as_str) != Some(app_id)
    {
        return Err(Error::Conflict("builder owner identifier mismatch".into()));
    }
    Ok(())
}
fn identity(kind: Kind, object: DynamicObject) -> Result<AppResourceIdentity> {
    let field = |value: Option<String>, name: &str| {
        value
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::ConfigurationError(format!("builder resource missing {name}")))
    };
    Ok(AppResourceIdentity {
        kind,
        name: field(object.metadata.name, "name")?,
        uid: field(object.metadata.uid, "UID")?,
        resource_version: Some(field(object.metadata.resource_version, "resourceVersion")?),
    })
}
fn deletion_params(receipt: &AppResourceIdentity) -> Result<DeleteParams> {
    let version = receipt
        .resource_version
        .clone()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::ConfigurationError("builder receipt lacks resourceVersion".into()))?;
    if receipt.uid.is_empty() {
        return Err(Error::ConfigurationError(
            "builder receipt lacks UID".into(),
        ));
    }
    Ok(DeleteParams {
        preconditions: Some(Preconditions {
            uid: Some(receipt.uid.clone()),
            resource_version: Some(version),
        }),
        propagation_policy: Some(PropagationPolicy::Foreground),
        ..Default::default()
    })
}
fn map_error(context: &str, error: kube::Error) -> Error {
    if matches!(&error, kube::Error::Api(response) if response.code == 409) {
        Error::Conflict(format!("{context}: {error}"))
    } else {
        Error::K8sError(format!("{context}: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn endpoint_inspection_stops_at_replaced_or_forbidden_workload() {
        for forbidden in [false, true] {
            let (runtime, server, requests) = adapter(forbidden).await;
            let context = shared_types::UserAppExecutionContext {
                app_id: "review".into(),
                user_id: "owner".into(),
                lifecycle_id: "life".into(),
                operation_id: "clear".into(),
                executor_id: "worker".into(),
                request_fingerprint: "a".repeat(64),
            };
            let snapshot = BuilderDeletionSnapshot {
                resource_binding: None,
                app_id: "review".into(),
                operation_id: "capture".into(),
                docker_bind_cleanup: false,
                resources: vec![AppResourceIdentity {
                    kind: Kind::StatefulSet,
                    name: "rcoder-app-builder-review".into(),
                    uid: "original-uid".into(),
                    resource_version: Some("1".into()),
                }],
            };
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                runtime.captured_builder_workspace(&snapshot, &context),
            )
            .await;
            server.abort();
            assert!(server.await.expect_err("fixture stopped").is_cancelled());
            assert!(result.expect("inspection deadline").is_err());
            let requests = requests.lock().expect("captured requests");
            assert_eq!(
                requests.len(),
                1,
                "must not query a replacement pod after rejection"
            );
            assert!(requests[0].starts_with(
                "GET /apis/apps/v1/namespaces/review-test/statefulsets/rcoder-app-builder-review "
            ));
        }
    }

    fn runtime(client: kube::Client) -> KubernetesRuntime {
        use super::super::kubernetes_runtime::KubernetesRuntimeConfig;
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
            },
            pod_cache: Default::default(),
            subvolume_path_cache: Default::default(),
        }
    }

    async fn adapter(
        forbidden: bool,
    ) -> (
        KubernetesRuntime,
        tokio::task::JoinHandle<()>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        drop(rustls::crypto::ring::default_provider().install_default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = requests.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = [0; 8192];
                let len = socket.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..len]).into_owned();
                let is_sts = request.contains("/statefulsets/");
                seen.lock().unwrap().push(request);
                let (status, body) = if forbidden {
                    (
                        403,
                        serde_json::json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"Forbidden","message":"denied","code":403}),
                    )
                } else if is_sts {
                    (
                        200,
                        serde_json::json!({"kind":"StatefulSet","apiVersion":"apps/v1","metadata":{"name":"rcoder-app-builder-review","uid":"replacement-uid","resourceVersion":"9","labels":{"rcoder.io/service-type":ServiceType::UserappBuilder.to_string(),"rcoder.io/identifier":"review"}}}),
                    )
                } else {
                    (
                        404,
                        serde_json::json!({"kind":"Status","apiVersion":"v1","status":"Failure","reason":"NotFound","message":"missing","code":404}),
                    )
                };
                let body = body.to_string();
                socket.write_all(format!("HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        });
        let client = kube::Client::try_from(kube::Config::new(
            format!("http://{address}").parse().unwrap(),
        ))
        .unwrap();
        (runtime(client), server, requests)
    }

    #[tokio::test]
    async fn builder_lookup_failure_is_not_absence() {
        let (runtime, server, _) = adapter(true).await;
        assert!(runtime.capture_builder("review").await.is_err());
        server.abort();
    }

    #[tokio::test]
    async fn replacement_builder_aborts_before_any_delete() {
        let (runtime, server, requests) = adapter(false).await;
        let mut snapshot = runtime.capture_builder("review").await.unwrap();
        snapshot.resources[0].uid = "captured-old-uid".into();
        assert!(matches!(
            runtime.delete_captured_builder(&snapshot).await,
            Err(Error::Conflict(_))
        ));
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| request.starts_with("GET "))
        );
        server.abort();
    }

    #[test]
    fn builder_delete_requires_both_physical_preconditions_and_foreground() {
        let mut receipt = AppResourceIdentity {
            kind: Kind::PersistentVolumeClaim,
            name: "pvc".into(),
            uid: "uid-a".into(),
            resource_version: Some("8".into()),
        };
        let body = serde_json::to_value(deletion_params(&receipt).unwrap()).unwrap();
        assert_eq!(body["preconditions"]["uid"], "uid-a");
        assert_eq!(body["preconditions"]["resourceVersion"], "8");
        assert_eq!(body["propagationPolicy"], "Foreground");
        receipt.resource_version = None;
        assert!(deletion_params(&receipt).is_err());
    }
}

#[cfg(test)]
mod workspace_endpoint_tests {
    use super::*;

    fn context() -> shared_types::UserAppExecutionContext {
        shared_types::UserAppExecutionContext {
            app_id: "review".into(),
            user_id: "owner".into(),
            lifecycle_id: "life".into(),
            operation_id: "clear".into(),
            executor_id: "worker".into(),
            request_fingerprint: "a".repeat(64),
        }
    }

    #[test]
    fn endpoint_requires_ready_pod_owned_by_the_captured_statefulset_and_lifecycle() {
        let context = context();
        let resource = AppResourceIdentity {
            kind: Kind::StatefulSet,
            name: "rcoder-app-builder-review".into(),
            uid: "captured-sts".into(),
            resource_version: Some("1".into()),
        };
        let original = serde_json::json!({
            "apiVersion":"v1", "kind":"Pod",
            "metadata": {
                "name":"rcoder-app-builder-review-0", "uid":"physical-pod",
                "annotations":context.resource_metadata(),
                "labels":{"rcoder.io/service-type":ServiceType::UserappBuilder.to_string(),"rcoder.io/identifier":"review"},
                "ownerReferences":[{"apiVersion":"apps/v1","kind":"StatefulSet","name":resource.name,"uid":resource.uid,"controller":true}]
            },
            "status":{"phase":"Running","podIP":"10.2.0.5","conditions":[{"type":"Ready","status":"True"}]}
        });
        let pod = serde_json::from_value(original.clone()).expect("pod fixture");
        let endpoint =
            workspace_endpoint_from_pod(&pod, &resource, &context).expect("physical pod");
        assert_eq!(endpoint.container_id, "physical-pod");
        assert_eq!(
            endpoint.base_url(),
            format!("http://10.2.0.5:{}", shared_types::AGENT_FILE_SERVER_PORT)
        );
        for (pointer, value) in [
            (
                "/metadata/ownerReferences/0/uid",
                serde_json::json!("recreated-sts"),
            ),
            (
                "/metadata/ownerReferences/0/controller",
                serde_json::json!(false),
            ),
            (
                "/metadata/annotations/rcoder.io~1owner-id",
                serde_json::json!("foreign-owner"),
            ),
            (
                "/metadata/annotations/rcoder.io~1lifecycle-id",
                serde_json::json!("next-life"),
            ),
            (
                "/metadata/labels/rcoder.io~1service-type",
                serde_json::json!(ServiceType::Userapp.to_string()),
            ),
            (
                "/metadata/labels/rcoder.io~1identifier",
                serde_json::json!("another-app"),
            ),
            ("/metadata/uid", serde_json::json!("")),
            ("/status/conditions/0/status", serde_json::json!("False")),
            ("/status/podIP", serde_json::json!("service.cluster.local")),
        ] {
            let mut changed = original.clone();
            *changed.pointer_mut(pointer).expect("fixture field") = value;
            let pod = serde_json::from_value(changed).expect("changed pod");
            assert!(
                workspace_endpoint_from_pod(&pod, &resource, &context).is_err(),
                "accepted changed field {pointer}"
            );
        }
    }
}
