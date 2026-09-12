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

    pub(super) async fn capture_builder(&self, app_id: &str) -> Result<BuilderDeletionSnapshot> {
        let family = ServiceType::UserappBuilder;
        let mut snapshot = BuilderDeletionSnapshot {
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

    pub(super) async fn claim_builder_storage(&self, app_id: &str) -> Result<()> {
        let name = self.workspace_pvc_name(app_id, &ServiceType::UserappBuilder)?;
        let api = self.builder_api(Kind::PersistentVolumeClaim)?;
        let object = api
            .get(&name)
            .await
            .map_err(|error| map_error("read builder storage claim", error))?;
        validate_owner(Kind::PersistentVolumeClaim, &object, app_id)?;
        if object.metadata.deletion_timestamp.is_some() {
            return Err(Error::Conflict("builder storage is terminating".into()));
        }
        let receipt = identity(Kind::PersistentVolumeClaim, object)?;
        let patch = serde_json::json!({"metadata":{"uid":receipt.uid,"resourceVersion":receipt.resource_version,"annotations":{"rcoder.io/storage-use-operation":uuid::Uuid::new_v4().to_string()}}});
        api.patch(&name, &PatchParams::default(), &Patch::Merge(patch))
            .await
            .map_err(|error| map_error("claim builder storage", error))?;
        Ok(())
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
