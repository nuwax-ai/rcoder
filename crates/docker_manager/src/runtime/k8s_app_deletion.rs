//! Identity-bound deletion. Never discover new deletion targets after capture.
use super::{k8s_pvc::K8sPvcOps, kubernetes_runtime::KubernetesRuntime};
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use kube::{
    Api,
    api::{DeleteParams, ListParams, Patch, PatchParams, Preconditions},
    core::{ApiResource, DynamicObject, GroupVersionKind},
};
use shared_types::{
    AppDeletionSnapshot, AppResourceIdentity, AppResourceKind as Kind, ServiceType,
};

impl KubernetesRuntime {
    fn deletion_api(&self, kind: Kind) -> Result<Api<DynamicObject>> {
        let (group, version, name) = match kind {
            Kind::Deployment => ("apps", "v1", "Deployment"),
            Kind::StatefulSet => ("apps", "v1", "StatefulSet"),
            Kind::Service => ("", "v1", "Service"),
            Kind::ConfigMap => ("", "v1", "ConfigMap"),
            Kind::Secret => ("", "v1", "Secret"),
            Kind::HttpRoute => ("gateway.networking.k8s.io", "v1", "HTTPRoute"),
            Kind::PersistentVolumeClaim => ("", "v1", "PersistentVolumeClaim"),
            Kind::Container => {
                return Err(Error::ConfigurationError(
                    "Docker identity passed to Kubernetes".into(),
                ));
            }
        };
        let resource = ApiResource::from_gvk(&GroupVersionKind::gvk(group, version, name));
        Ok(Api::namespaced_with(
            self.client.clone(),
            &self.namespace,
            &resource,
        ))
    }

    pub(super) async fn capture_deletion(
        &self,
        app_id: &str,
        expected: Option<&str>,
    ) -> Result<AppDeletionSnapshot> {
        let mut snapshot = AppDeletionSnapshot {
            app_id: app_id.into(),
            operation_id: uuid::Uuid::new_v4().to_string(),
            resources: vec![],
        };
        for name in [
            self.workspace_pvc_name(app_id, &ServiceType::Userapp)?,
            self.app_data_pvc_name(app_id)?,
        ] {
            match self
                .deletion_api(Kind::PersistentVolumeClaim)?
                .get(&name)
                .await
            {
                Ok(object) => {
                    ensure_userapp_pvc(&object)?;
                    snapshot
                        .resources
                        .push(identity(Kind::PersistentVolumeClaim, object)?);
                }
                Err(kube::Error::Api(error)) if error.code == 404 => {}
                Err(error) => return Err(map_error("capture application storage", error)),
            }
        }
        let selector = format!(
            "app.kubernetes.io/instance={app_id},app.kubernetes.io/managed-by=rcoder-app-manager"
        );
        for kind in [
            Kind::Deployment,
            Kind::Service,
            Kind::ConfigMap,
            Kind::Secret,
            Kind::HttpRoute,
        ] {
            let list = match self
                .deletion_api(kind)?
                .list(&ListParams::default().labels(&selector))
                .await
            {
                Ok(list) => list,
                Err(kube::Error::Api(error)) if error.code == 404 && kind == Kind::HttpRoute => {
                    continue;
                }
                Err(error) => return Err(map_error("capture application resources", error)),
            };
            for object in list.items {
                snapshot.resources.push(identity(kind, object)?);
            }
        }
        if let Some(expected) = expected {
            let actual = snapshot
                .resources
                .iter()
                .find(|r| r.kind == Kind::Deployment)
                .and_then(|r| r.resource_version.as_deref());
            if actual != Some(expected) {
                return Err(Error::Conflict(
                    "deployment changed before deletion capture".into(),
                ));
            }
        }
        Ok(snapshot)
    }

    pub(super) async fn delete_captured(
        &self,
        snapshot: &AppDeletionSnapshot,
        storage: bool,
    ) -> Result<()> {
        for resource in snapshot
            .resources
            .iter()
            .filter(|r| (r.kind == Kind::PersistentVolumeClaim) == storage)
        {
            let version = resource
                .resource_version
                .clone()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    Error::ConfigurationError("captured Kubernetes resource has no version".into())
                })?;
            if resource.uid.is_empty() {
                return Err(Error::ConfigurationError(
                    "captured resource has no UID".into(),
                ));
            }
            let api = self.deletion_api(resource.kind)?;
            let params = DeleteParams {
                preconditions: Some(Preconditions {
                    uid: Some(resource.uid.clone()),
                    resource_version: Some(version),
                }),
                ..Default::default()
            };
            match api.delete(&resource.name, &params).await {
                Ok(_) => {}
                Err(kube::Error::Api(error)) if error.code == 404 => {}
                Err(error) => return Err(map_error("delete captured application resource", error)),
            }
            if storage {
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
                loop {
                    match api.get(&resource.name).await {
                        Err(kube::Error::Api(error)) if error.code == 404 => break,
                        Ok(current) if current.metadata.uid.as_deref() != Some(&resource.uid) => {
                            return Err(Error::Conflict(
                                "storage was replaced while deletion completed".into(),
                            ));
                        }
                        Ok(_) => {}
                        Err(error) => {
                            return Err(map_error("observe captured storage deletion", error));
                        }
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(Error::Timeout(
                            "captured storage is still terminating".into(),
                        ));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                self.subvolume_path_cache
                    .write()
                    .await
                    .remove(&resource.name);
            }
        }
        Ok(())
    }

    /// Every writer that reuses storage advances its CAS version before starting a pod.
    pub(super) async fn claim_app_storage(&self, app_id: &str) -> Result<()> {
        for (index, name) in [
            self.workspace_pvc_name(app_id, &ServiceType::Userapp)?,
            self.app_data_pvc_name(app_id)?,
        ]
        .into_iter()
        .enumerate()
        {
            let api = self.deletion_api(Kind::PersistentVolumeClaim)?;
            let pvc = match api.get(&name).await {
                Ok(pvc) => pvc,
                Err(kube::Error::Api(error)) if error.code == 404 && index == 1 => continue,
                Err(error) => return Err(map_error("read app storage claim", error)),
            };
            ensure_userapp_pvc(&pvc)?;
            if pvc.metadata.deletion_timestamp.is_some() {
                return Err(Error::Conflict("app storage is terminating".into()));
            }
            let uid = pvc
                .metadata
                .uid
                .ok_or_else(|| Error::ConfigurationError("app storage has no UID".into()))?;
            let version = pvc.metadata.resource_version.ok_or_else(|| {
                Error::ConfigurationError("app storage has no resourceVersion".into())
            })?;
            let patch = serde_json::json!({"metadata":{"uid":uid,"resourceVersion":version,"annotations":{"rcoder.io/storage-use-operation":uuid::Uuid::new_v4().to_string()}}});
            api.patch(&name, &PatchParams::default(), &Patch::Merge(patch))
                .await
                .map_err(|error| map_error("claim app storage", error))?;
        }
        Ok(())
    }
}

fn identity(kind: Kind, object: DynamicObject) -> Result<AppResourceIdentity> {
    Ok(AppResourceIdentity {
        kind,
        name: object
            .metadata
            .name
            .ok_or_else(|| Error::ConfigurationError("resource has no name".into()))?,
        uid: object
            .metadata
            .uid
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Error::ConfigurationError("resource has no UID".into()))?,
        resource_version: Some(
            object
                .metadata
                .resource_version
                .filter(|v| !v.is_empty())
                .ok_or_else(|| Error::ConfigurationError("resource has no version".into()))?,
        ),
    })
}

fn ensure_userapp_pvc(pvc: &DynamicObject) -> Result<()> {
    let owner = pvc
        .metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get("service_type"));
    if owner.map(String::as_str) != Some(ServiceType::Userapp.to_string().as_str()) {
        return Err(Error::Conflict(
            "PVC does not belong to the UserApp production service family".into(),
        ));
    }
    Ok(())
}

fn map_error(context: &str, error: kube::Error) -> Error {
    match &error {
        kube::Error::Api(response) if response.code == 409 => {
            Error::Conflict(format!("{context}: {error}"))
        }
        _ => Error::K8sError(format!("{context}: {error}")),
    }
}
