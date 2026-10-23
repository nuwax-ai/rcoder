//! Default-template recovery only reuses captured, namespace-local managed PVCs.
//! The durable operation owns every metadata write; no volume is deleted.
use super::{k8s_pod::K8sPodOps, k8s_pvc::K8sPvcOps, kubernetes_runtime::KubernetesRuntime};
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use k8s_openapi::api::{
    apps::v1::StatefulSet,
    core::v1::{PersistentVolumeClaim, Pod, Service},
};
use kube::{
    Api,
    api::{ListParams, Patch, PatchParams},
};
use shared_types::{
    AppResourceIdentity, AppResourceKind, ServiceType, UserAppExecutionContext,
    UserAppOperationScope, UserAppRetainedStorage,
};

fn family(scope: UserAppOperationScope) -> Result<ServiceType> {
    match scope {
        UserAppOperationScope::Dev => Ok(ServiceType::UserappBuilder),
        UserAppOperationScope::Prod => Ok(ServiceType::Userapp),
        UserAppOperationScope::Application => Err(Error::ConfigurationError(
            "Recovery requires dev or prod".into(),
        )),
    }
}
fn inspect_pvc(
    pvc: &PersistentVolumeClaim,
    expected: &str,
    service: &ServiceType,
    app_id: &str,
) -> Result<Option<String>> {
    let m = &pvc.metadata;
    let labels = m.labels.as_ref();
    if m.name.as_deref() != Some(expected)
        || m.uid.as_deref().is_none_or(str::is_empty)
        || m.resource_version.as_deref().is_none_or(str::is_empty)
        || m.deletion_timestamp.is_some()
        || labels
            .and_then(|l| l.get("app.kubernetes.io/managed-by"))
            .map(String::as_str)
            != Some("rcoder-runtime")
        || labels
            .and_then(|l| l.get("service_type"))
            .map(String::as_str)
            != Some(service.to_string().as_str())
    {
        return Err(Error::Conflict(format!(
            "Retained PVC {expected} is not an active managed application volume"
        )));
    }
    if let Some(annotations) = &m.annotations {
        if annotations
            .get("rcoder.io/application-id")
            .is_some_and(|id| id != app_id)
        {
            return Err(Error::Conflict(
                "Retained PVC belongs to another application".into(),
            ));
        }
        if let Some(id) = annotations.get("rcoder.io/lifecycle-id") {
            shared_types::validate_identifier(id, "lifecycle_id").map_err(Error::Conflict)?;
            if annotations
                .get("rcoder.io/application-id")
                .map(String::as_str)
                != Some(app_id)
            {
                return Err(Error::Conflict(
                    "Retained PVC lifecycle has no matching application identity".into(),
                ));
            }
            return Ok(Some(id.clone()));
        }
    }
    Ok(None)
}

fn inspect_service(
    service: &Service,
    app_id: &str,
    scope: UserAppOperationScope,
    headless: bool,
) -> Result<Option<String>> {
    let metadata = &service.metadata;
    if metadata.uid.as_deref().is_none_or(str::is_empty)
        || metadata
            .resource_version
            .as_deref()
            .is_none_or(str::is_empty)
        || metadata.deletion_timestamp.is_some()
    {
        return Err(Error::Conflict(
            "Retained Service identity is incomplete or deleting".into(),
        ));
    }
    match scope {
        UserAppOperationScope::Dev => {
            super::k8s_service::validate_builder_service(service, app_id, headless)?
        }
        UserAppOperationScope::Prod => {
            let labels = metadata.labels.as_ref();
            let spec = service
                .spec
                .as_ref()
                .ok_or_else(|| Error::Conflict("Retained production Service has no spec".into()))?;
            if labels
                .and_then(|l| l.get("app.kubernetes.io/managed-by"))
                .map(String::as_str)
                != Some("rcoder-app-manager")
                || labels
                    .and_then(|l| l.get("rcoder.io/app-id"))
                    .map(String::as_str)
                    != Some(app_id)
                || spec
                    .selector
                    .as_ref()
                    .and_then(|l| l.get("rcoder.io/app-id"))
                    .map(String::as_str)
                    != Some(app_id)
            {
                return Err(Error::Conflict(
                    "Retained production Service ownership or routing changed".into(),
                ));
            }
        }
        UserAppOperationScope::Application => {
            return Err(Error::ConfigurationError(
                "Invalid Service recovery scope".into(),
            ));
        }
    }
    let annotations = metadata.annotations.as_ref();
    if annotations
        .and_then(|a| a.get("rcoder.io/application-id"))
        .is_some_and(|id| id != app_id)
    {
        return Err(Error::Conflict(
            "Retained Service belongs to another application".into(),
        ));
    }
    Ok(annotations
        .and_then(|a| a.get("rcoder.io/lifecycle-id"))
        .cloned())
}

impl KubernetesRuntime {
    async fn recovery_controller_exists(
        &self,
        app_id: &str,
        scope: UserAppOperationScope,
    ) -> Result<bool> {
        match scope {
            UserAppOperationScope::Dev => Ok(Api::<StatefulSet>::namespaced(
                self.client.clone(),
                &self.namespace,
            )
            .get_opt(&self.pod_name(app_id, &ServiceType::UserappBuilder)?)
            .await
            .map_err(|e| Error::K8sError(e.to_string()))?
            .is_some()),
            UserAppOperationScope::Prod => Ok(self
                .deployments_api()
                .get_opt(&self.app_deployment_name(app_id))
                .await
                .map_err(|e| Error::K8sError(e.to_string()))?
                .is_some()),
            UserAppOperationScope::Application => {
                Err(Error::ConfigurationError("Invalid recovery scope".into()))
            }
        }
    }

    pub(super) async fn missing_compute_storage_inventory(
        &self,
        app_id: &str,
        scope: UserAppOperationScope,
    ) -> Result<Option<UserAppRetainedStorage>> {
        shared_types::validate_identifier(app_id, "app_id").map_err(Error::ConfigurationError)?;
        // Existing controllers use the normal bound reuse/restart path. Their
        // stable Services may predate lifecycle annotations; that alone must
        // never turn an ordinary ensure into legacy storage adoption.
        if self.recovery_controller_exists(app_id, scope).await? {
            return Ok(None);
        }
        self.retained_storage_inventory(app_id, scope).await
    }

    async fn recovery_compute_absent(
        &self,
        app_id: &str,
        scope: UserAppOperationScope,
        volumes: &[AppResourceIdentity],
    ) -> Result<()> {
        if self.recovery_controller_exists(app_id, scope).await? {
            return Err(Error::Conflict(
                "Retained storage still has a compute controller; reconcile the original execution"
                    .into(),
            ));
        }
        let pods = Api::<Pod>::namespaced(self.client.clone(), &self.namespace)
            .list(&ListParams::default())
            .await
            .map_err(|e| Error::K8sError(format!("Inspect retained PVC consumers: {e}")))?;
        for pod in pods.items {
            if pod.spec.as_ref().is_some_and(|spec| {
                spec.volumes.as_deref().unwrap_or_default().iter().any(|v| {
                    v.persistent_volume_claim.as_ref().is_some_and(|claim| {
                        volumes
                            .iter()
                            .any(|expected| expected.name == claim.claim_name)
                    })
                })
            }) {
                return Err(Error::Conflict(
                    "Retained PVC is still referenced by a Pod; wait for its original cleanup"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    pub(super) async fn verify_absent_retained_builder(
        &self,
        context: &UserAppExecutionContext,
    ) -> Result<bool> {
        use k8s_openapi::api::coordination::v1::Lease;
        let Some(storage) = self
            .retained_storage_inventory(&context.app_id, UserAppOperationScope::Dev)
            .await?
        else {
            return Ok(false);
        };
        if storage
            .lifecycle_id
            .as_ref()
            .is_some_and(|id| id != &context.lifecycle_id)
        {
            return Ok(false);
        }
        self.recovery_compute_absent(
            &context.app_id,
            UserAppOperationScope::Dev,
            &storage.volumes,
        )
        .await?;
        let selector = format!(
            "rcoder.io/operation-app={},rcoder.io/operation-family={}",
            context.app_id,
            ServiceType::UserappBuilder
        );
        let leases = Api::<Lease>::namespaced(self.client.clone(), &self.namespace)
            .list(&ListParams::default().labels(&selector))
            .await
            .map_err(|e| Error::K8sError(e.to_string()))?;
        for lease in leases.items {
            let Some(spec) = lease.spec else {
                return Ok(false);
            };
            let Some(renew) = spec.renew_time else {
                return Ok(false);
            };
            let age = (k8s_openapi::jiff::Timestamp::now() - renew.0).get_seconds();
            if age <= i64::from(spec.lease_duration_seconds.unwrap_or(60)) {
                return Ok(false);
            }
        }
        self.recovery_compute_absent(
            &context.app_id,
            UserAppOperationScope::Dev,
            &storage.volumes,
        )
        .await?;
        Ok(true)
    }

    pub(super) async fn retained_storage_inventory(
        &self,
        app_id: &str,
        scope: UserAppOperationScope,
    ) -> Result<Option<UserAppRetainedStorage>> {
        shared_types::validate_identifier(app_id, "app_id").map_err(Error::ConfigurationError)?;
        let service = family(scope)?;
        let name = self.workspace_pvc_name(app_id, &service)?;
        let api = Api::<PersistentVolumeClaim>::namespaced(self.client.clone(), &self.namespace);
        let Some(pvc) = api
            .get_opt(&name)
            .await
            .map_err(|e| Error::K8sError(format!("Read retained workspace: {e}")))?
        else {
            return Ok(None);
        };
        let mut lifecycle_id = inspect_pvc(&pvc, &name, &service, app_id)?;
        let mut volumes = vec![AppResourceIdentity {
            kind: AppResourceKind::PersistentVolumeClaim,
            name,
            uid: pvc
                .metadata
                .uid
                .clone()
                .ok_or_else(|| Error::Conflict("PVC UID missing".into()))?,
            resource_version: pvc.metadata.resource_version.clone(),
        }];
        // Older production layouts may have a separate data PVC. It is retained too.
        if scope == UserAppOperationScope::Prod {
            let name = self.app_data_pvc_name(app_id)?;
            if let Some(data) = api
                .get_opt(&name)
                .await
                .map_err(|e| Error::K8sError(e.to_string()))?
            {
                let id = inspect_pvc(&data, &name, &service, app_id)?;
                if lifecycle_id
                    .as_ref()
                    .zip(id.as_ref())
                    .is_some_and(|(a, b)| a != b)
                {
                    return Err(Error::Conflict(
                        "Retained volume lifecycles disagree".into(),
                    ));
                }
                lifecycle_id = lifecycle_id.or(id);
                volumes.push(AppResourceIdentity {
                    kind: AppResourceKind::PersistentVolumeClaim,
                    name,
                    uid: data
                        .metadata
                        .uid
                        .ok_or_else(|| Error::Conflict("Data PVC UID missing".into()))?,
                    resource_version: data.metadata.resource_version,
                });
            }
        }
        // Never choose a lifecycle that conflicts with the other environment.
        let peer = match scope {
            UserAppOperationScope::Dev => ServiceType::Userapp,
            UserAppOperationScope::Prod => ServiceType::UserappBuilder,
            UserAppOperationScope::Application => {
                return Err(Error::ConfigurationError("Invalid recovery scope".into()));
            }
        };
        let peer_name = self.workspace_pvc_name(app_id, &peer)?;
        if let Some(peer_pvc) = api
            .get_opt(&peer_name)
            .await
            .map_err(|e| Error::K8sError(e.to_string()))?
        {
            let peer_id = inspect_pvc(&peer_pvc, &peer_name, &peer, app_id)?;
            if lifecycle_id
                .as_ref()
                .zip(peer_id.as_ref())
                .is_some_and(|(a, b)| a != b)
            {
                return Err(Error::Conflict(
                    "Dev/prod retained lifecycle identities disagree".into(),
                ));
            }
            lifecycle_id = lifecycle_id.or(peer_id);
        }
        let service_api = Api::<Service>::namespaced(self.client.clone(), &self.namespace);
        let mut services = Vec::new();
        for service_scope in [UserAppOperationScope::Dev, UserAppOperationScope::Prod] {
            let names = match service_scope {
                UserAppOperationScope::Dev => {
                    let prefix = self.pod_name(app_id, &ServiceType::UserappBuilder)?;
                    vec![
                        (format!("{prefix}-svc"), false),
                        (format!("{prefix}-headless"), true),
                    ]
                }
                UserAppOperationScope::Prod => vec![(self.app_service_name(app_id), false)],
                UserAppOperationScope::Application => {
                    return Err(Error::ConfigurationError("Invalid recovery scope".into()));
                }
            };
            for (name, headless) in names {
                if let Some(service) = service_api
                    .get_opt(&name)
                    .await
                    .map_err(|e| Error::K8sError(e.to_string()))?
                {
                    let id = inspect_service(&service, app_id, service_scope, headless)?;
                    if lifecycle_id
                        .as_ref()
                        .zip(id.as_ref())
                        .is_some_and(|(a, b)| a != b)
                    {
                        return Err(Error::Conflict(
                            "Retained Service and PVC lifecycles disagree".into(),
                        ));
                    }
                    lifecycle_id = lifecycle_id.or(id);
                    if service_scope == scope {
                        services.push(AppResourceIdentity {
                            kind: AppResourceKind::Service,
                            name,
                            uid: service.metadata.uid.ok_or_else(|| {
                                Error::Conflict("Retained Service UID missing".into())
                            })?,
                            resource_version: service.metadata.resource_version,
                        });
                    }
                }
            }
        }
        let inventory = UserAppRetainedStorage {
            app_id: app_id.into(),
            scope,
            lifecycle_id,
            volumes,
            services,
        };
        inventory.validate().map_err(Error::ConfigurationError)?;
        Ok(Some(inventory))
    }

    pub(super) async fn recover_retained_storage(
        &self,
        context: &UserAppExecutionContext,
        captured: &UserAppRetainedStorage,
    ) -> Result<()> {
        captured.validate().map_err(Error::ConfigurationError)?;
        context
            .validate_identity(&captured.app_id)
            .map_err(Error::Conflict)?;
        if captured
            .lifecycle_id
            .as_ref()
            .is_some_and(|id| id != &context.lifecycle_id)
        {
            return Err(Error::Conflict("Captured storage lifecycle changed".into()));
        }
        let service = family(captured.scope)?;
        let current = self
            .retained_storage_inventory(&captured.app_id, captured.scope)
            .await?
            .ok_or_else(|| Error::Conflict("Captured recovery workspace disappeared".into()))?;
        if current
            .lifecycle_id
            .as_ref()
            .is_some_and(|id| id != &context.lifecycle_id)
            || current.services.len() != captured.services.len()
            || current
                .services
                .iter()
                .zip(&captured.services)
                .any(|(a, b)| a.name != b.name || a.uid != b.uid)
            || current.volumes.len() != captured.volumes.len()
            || current
                .volumes
                .iter()
                .zip(&captured.volumes)
                .any(|(a, b)| a.name != b.name || a.uid != b.uid)
        {
            return Err(Error::Conflict(
                "Captured recovery storage inventory changed".into(),
            ));
        }
        let api = Api::<PersistentVolumeClaim>::namespaced(self.client.clone(), &self.namespace);
        for expected in &captured.volumes {
            let mut finished = false;
            for _attempt in 0..4 {
                let pvc = api
                    .get(&expected.name)
                    .await
                    .map_err(|e| Error::K8sError(e.to_string()))?;
                let current_lifecycle =
                    inspect_pvc(&pvc, &expected.name, &service, &captured.app_id)?;
                if pvc.metadata.uid.as_deref() != Some(&expected.uid) {
                    return Err(Error::Conflict(
                        "Retained PVC was replaced during recovery".into(),
                    ));
                }
                if let Some(id) = current_lifecycle {
                    if id != context.lifecycle_id {
                        return Err(Error::Conflict(
                            "Retained PVC lifecycle changed during recovery".into(),
                        ));
                    }
                    finished = true;
                    break;
                }
                self.recovery_compute_absent(&captured.app_id, captured.scope, &captured.volumes)
                    .await?;
                let patch = serde_json::json!({"metadata":{"uid":expected.uid,"resourceVersion":pvc.metadata.resource_version,
                    "annotations":context.resource_metadata()}});
                match api
                    .patch(
                        &expected.name,
                        &PatchParams::default(),
                        &Patch::Merge(patch),
                    )
                    .await
                {
                    Ok(_) => {
                        finished = true;
                        break;
                    }
                    Err(kube::Error::Api(error)) if error.code == 409 => continue,
                    Err(error) => {
                        return Err(Error::K8sError(format!("Bind retained storage: {error}")));
                    }
                }
            }
            if !finished {
                return Err(Error::Conflict(
                    "Retained PVC changed repeatedly during recovery".into(),
                ));
            }
        }
        let service_api = Api::<Service>::namespaced(self.client.clone(), &self.namespace);
        for expected in &captured.services {
            let service = service_api
                .get(&expected.name)
                .await
                .map_err(|e| Error::K8sError(e.to_string()))?;
            let current_lifecycle = inspect_service(
                &service,
                &captured.app_id,
                captured.scope,
                expected.name.ends_with("-headless"),
            )?;
            if service.metadata.uid.as_deref() != Some(&expected.uid) {
                return Err(Error::Conflict("Captured Service was replaced".into()));
            }
            if let Some(id) = current_lifecycle {
                if id != context.lifecycle_id {
                    return Err(Error::Conflict("Captured Service lifecycle changed".into()));
                }
                continue;
            }
            self.recovery_compute_absent(&captured.app_id, captured.scope, &captured.volumes)
                .await?;
            service_api.patch(&expected.name,&PatchParams::default(),&Patch::Merge(serde_json::json!({"metadata":{
                "uid":expected.uid,"resourceVersion":service.metadata.resource_version,"annotations":context.resource_metadata()
            }}))).await.map_err(|e|Error::K8sError(format!("Bind retained Service: {e}")))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_volume_requires_managed_family_and_never_accepts_foreign_identity() {
        let mut pvc: PersistentVolumeClaim = serde_json::from_value(serde_json::json!({"metadata":{"name":"rcoder-app-51-workspace","uid":"old-volume","resourceVersion":"1","labels":{"app.kubernetes.io/managed-by":"rcoder-runtime","service_type":"user-app"}}})).unwrap();
        assert_eq!(
            inspect_pvc(&pvc, "rcoder-app-51-workspace", &ServiceType::Userapp, "51").unwrap(),
            None
        );
        pvc.metadata.annotations = Some(std::collections::BTreeMap::from([(
            "rcoder.io/application-id".into(),
            "other".into(),
        )]));
        assert!(inspect_pvc(&pvc, "rcoder-app-51-workspace", &ServiceType::Userapp, "51").is_err());
        pvc.metadata.annotations = None;
        assert!(
            inspect_pvc(
                &pvc,
                "rcoder-app-51-workspace",
                &ServiceType::UserappBuilder,
                "51"
            )
            .is_err()
        );
        pvc.metadata.labels = None;
        assert!(inspect_pvc(&pvc, "rcoder-app-51-workspace", &ServiceType::Userapp, "51").is_err());
    }
}

#[cfg(test)]
#[path = "k8s_default_recovery_tests.rs"]
mod wire_tests;
