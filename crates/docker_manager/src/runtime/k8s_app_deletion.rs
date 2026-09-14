//! Identity-bound deletion. Never discover new deletion targets after capture.
use super::{k8s_pvc::K8sPvcOps, kubernetes_runtime::KubernetesRuntime};
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use kube::{
    Api,
    api::{DeleteParams, ListParams, Patch, PatchParams, Preconditions, PropagationPolicy},
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
                propagation_policy: Some(PropagationPolicy::Foreground),
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
            // DELETE acknowledgement is not disappearance. Confirm every
            // captured kind before permitting the next destructive boundary.
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
            tokio::time::timeout_at(deadline, async {
                loop {
                    match api.get(&resource.name).await {
                        Err(kube::Error::Api(error)) if error.code == 404 => return Ok(()),
                        Ok(current) if current.metadata.uid.as_deref() != Some(&resource.uid) => {
                            return Err(Error::Conflict(
                                "application resource was replaced while deletion completed".into(),
                            ));
                        }
                        Ok(_) => {}
                        Err(error) => {
                            return Err(map_error("observe captured resource deletion", error));
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            })
            .await
            .map_err(|_| {
                Error::Timeout("captured application resource is still terminating".into())
            })??;
            if storage {
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
        self.claim_app_storage_with_context(app_id, None).await
    }

    pub(super) async fn claim_app_storage_with_context(
        &self,
        app_id: &str,
        context: Option<&shared_types::UserAppExecutionContext>,
    ) -> Result<()> {
        let operation_id = context
            .map(|context| context.operation_id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        for (index, name) in [
            self.workspace_pvc_name(app_id, &ServiceType::Userapp)?,
            self.app_data_pvc_name(app_id)?,
        ]
        .into_iter()
        .enumerate()
        {
            let api = self.deletion_api(Kind::PersistentVolumeClaim)?;
            // spec §5：PATCH 明确 409 且重读 UID 不变、未删除才重试（4 次）。
            // PVC resourceVersion 会被控制面（绑定/扩容/卷挂载记账）并发 bump，
            // 单发条件写对首个 PATCH 撞 409 过敏（K8s 实测抓出）。
            let mut attempts = 0u32;
            let claim_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            let mut claim_uid: Option<String> = None;
            loop {
                attempts += 1;
                let pvc = match api.get(&name).await {
                    Ok(pvc) => pvc,
                    Err(kube::Error::Api(error)) if error.code == 404 && index == 1 => break,
                    Err(error) => return Err(map_error("read app storage claim", error)),
                };
                ensure_userapp_pvc(&pvc)?;
                if let Some(context) = context {
                    context
                        .validate_identity(app_id, Some(&context.user_id))
                        .map_err(Error::ConfigurationError)?;
                    let annotations = pvc.metadata.annotations.as_ref().ok_or_else(|| {
                        Error::Conflict(
                            "Application storage requires explicit lifecycle adoption".into(),
                        )
                    })?;
                    context
                        .validate_application_metadata(annotations)
                        .map_err(Error::Conflict)?;
                }

                if pvc.metadata.deletion_timestamp.is_some() {
                    return Err(Error::Conflict("app storage is terminating".into()));
                }
                let uid = pvc
                    .metadata
                    .uid
                    .ok_or_else(|| Error::ConfigurationError("app storage has no UID".into()))?;
                match &claim_uid {
                    None => claim_uid = Some(uid.clone()),
                    Some(previous) if previous != &uid => {
                        return Err(Error::Conflict(
                            "app storage was replaced while claiming".into(),
                        ));
                    }
                    _ => {}
                }
                let version = pvc.metadata.resource_version.ok_or_else(|| {
                    Error::ConfigurationError("app storage has no resourceVersion".into())
                })?;
                let patch = serde_json::json!({"metadata":{"uid":uid,"resourceVersion":version,"annotations":{"rcoder.io/storage-use-operation":operation_id}}});
                match api
                    .patch(&name, &PatchParams::default(), &Patch::Merge(patch))
                    .await
                {
                    Ok(_) => break,
                    // spec §5：4 次/3 秒总预算——次数与总时限双限（GET/PATCH
                    // 各自的等待时间计入预算，超时即上抛由上层裁决）。
                    Err(kube::Error::Api(conflict))
                        if conflict.code == 409
                            && attempts < 4
                            && std::time::Instant::now() < claim_deadline =>
                    {
                        tracing::warn!(
                            pvc = %name,
                            attempt = attempts,
                            "[K8S] app storage claim 409 (concurrent resourceVersion bump); re-reading and retrying"
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    }
                    Err(error) => return Err(map_error("claim app storage", error)),
                }
            }
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
        // 403 是 API server 的确定性拒绝（RBAC/准入），无在途结果——结构化
        // 保留状态码供确定性分类；对外码不变（非 409 的 RuntimeRejected 仍
        // 映射 ERR_BACKEND_ERROR，与旧 K8sError→Backend 一致）。
        kube::Error::Api(response) if response.code == 403 => {
            Error::RequestRejected(shared_types::RuntimeRequestRejection {
                status: 403,
                message: format!("{context}: {error}"),
            })
        }
        _ => Error::K8sError(format!("{context}: {error}")),
    }
}
