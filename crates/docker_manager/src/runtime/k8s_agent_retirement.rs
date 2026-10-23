//! Captured compute retirement and resource discovery. PVCs are never targets.
use container_runtime_api::{
    AgentResourceIdentity as Identity, AgentResourceKind as Kind, AgentServiceCleanupFailure,
    AgentStopOutcome, AgentStopSnapshot, ContainerRuntimeError as Error,
    ContainerRuntimeResult as Result,
};
use k8s_openapi::{
    api::{
        apps::v1::StatefulSet,
        core::v1::{PersistentVolumeClaim, Pod, Service},
    },
    apimachinery::pkg::apis::meta::v1::{ListMeta, ObjectMeta},
};
use kube::{
    Api, Resource,
    api::{DeleteParams, ListParams, Patch, PatchParams, Preconditions},
    runtime::wait::{await_condition, conditions},
};
use serde::{Serialize, de::DeserializeOwned};
use shared_types::ServiceType;
use std::{collections::BTreeMap, fmt::Debug};
use tokio::time::Instant;

use super::kubernetes_runtime::{KubernetesRuntime, RUNTIME_MANAGED_LABEL};

mod events;
#[cfg(test)]
mod tests;

const RETIREMENT_BUDGET: std::time::Duration = std::time::Duration::from_secs(75);
const FENCE_ANNOTATION: &str = "rcoder.io/agent-admission-fence";

fn api_error(error: kube::Error) -> Error {
    Error::K8sError(format!("Agent retirement: {error}"))
}
fn conflict(message: &str) -> Error {
    Error::Conflict(message.into())
}

/// An unrequested server-side page is still partial evidence. Callers must
/// preserve resources instead of treating the first page as authoritative absence.
fn require_complete_list(metadata: &ListMeta, kind: &str) -> Result<()> {
    if metadata
        .continue_
        .as_ref()
        .is_some_and(|value| !value.is_empty())
        || metadata
            .remaining_item_count
            .is_some_and(|remaining| remaining > 0)
    {
        return Err(Error::K8sError(format!(
            "Agent retirement observed an incomplete {kind} list; resources are retained"
        )));
    }
    Ok(())
}

fn scope(metadata: &ObjectMeta) -> Option<(ServiceType, String)> {
    let labels = metadata.labels.as_ref()?;
    if labels
        .get("app.kubernetes.io/managed-by")
        .map(String::as_str)
        != Some("rcoder-runtime")
    {
        return None;
    }
    let family = labels
        .get("rcoder.io/service-type")?
        .parse::<ServiceType>()
        .ok()?;
    match family {
        ServiceType::Userapp => None,
        ServiceType::WebAgentRunner
        | ServiceType::ComputerAgentRunner
        | ServiceType::ComputerNormalProject
        | ServiceType::UserappBuilder => {
            let identifier = labels
                .get("rcoder.io/identifier")
                .or_else(|| labels.get("app.kubernetes.io/instance"))
                .filter(|value| !value.is_empty())?
                .clone();
            Some((family.family_representative(), identifier))
        }
    }
}

fn expected_scope(
    metadata: &ObjectMeta,
    namespace: &str,
    identifier: &str,
    family: ServiceType,
) -> bool {
    let Some(labels) = metadata.labels.as_ref() else {
        return false;
    };
    metadata.namespace.as_deref() == Some(namespace)
        && labels
            .get("app.kubernetes.io/managed-by")
            .map(String::as_str)
            == Some("rcoder-runtime")
        && labels
            .get("rcoder.io/identifier")
            .or_else(|| labels.get("app.kubernetes.io/instance"))
            .is_none_or(|value| value == identifier)
        && labels.get("rcoder.io/service-type").is_none_or(|value| {
            value
                .parse::<ServiceType>()
                .is_ok_and(|parsed| parsed.same_container_family(family))
        })
}

/// A current deletion marker authorizes only observation of the original UID.
/// Its own RV advance must never become a refreshed DELETE precondition.
fn already_terminating(
    snapshot: &AgentStopSnapshot,
    target: &Identity,
    metadata: &ObjectMeta,
) -> bool {
    metadata.name.as_deref() == Some(target.name.as_str())
        && metadata.uid.as_deref() == Some(target.uid.as_str())
        && metadata.deletion_timestamp.is_some()
        && expected_scope(
            metadata,
            snapshot.namespace.as_deref().unwrap_or_default(),
            &snapshot.identifier,
            snapshot.service_type,
        )
}

fn identity(kind: Kind, metadata: &ObjectMeta) -> Result<Identity> {
    let name = metadata
        .name
        .clone()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| conflict("Agent resource name is missing"))?;
    let uid = metadata
        .uid
        .clone()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| conflict("Agent resource UID is missing"))?;
    let resource_version = metadata
        .resource_version
        .clone()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| conflict("Agent resource version is missing"))?;
    Ok(Identity {
        kind,
        name,
        uid,
        resource_version: Some(resource_version),
        created_at: metadata
            .creation_timestamp
            .as_ref()
            .map(|time| {
                chrono::DateTime::parse_from_rfc3339(&time.0.to_string())
                    .map(|parsed| parsed.with_timezone(&chrono::Utc))
            })
            .transpose()
            .map_err(|error| {
                Error::K8sError(format!("Read Agent resource creation time: {error}"))
            })?,
        owner_uid: metadata
            .owner_references
            .as_ref()
            .and_then(|owners| owners.iter().find(|owner| owner.controller == Some(true)))
            .map(|owner| owner.uid.clone()),
    })
}

fn validate_pod_controller(
    snapshot: &AgentStopSnapshot,
    pod: &Pod,
    canonical_workload_name: &str,
) -> Result<()> {
    let mut owners = pod
        .metadata
        .owner_references
        .iter()
        .flatten()
        .filter(|owner| owner.controller == Some(true));
    let owner = owners.next();
    if owners.next().is_some() {
        return Err(conflict("Agent Pod has multiple controllers"));
    }
    match (snapshot.controller.as_ref(), owner) {
        (Some(controller), Some(owner))
            if owner.kind == "StatefulSet"
                && owner.name == controller.name
                && owner.uid == controller.uid =>
        {
            Ok(())
        }
        (None, Some(owner))
            if owner.kind == "StatefulSet"
                && owner.name == canonical_workload_name
                && !owner.uid.is_empty()
                && pod.metadata.name.as_deref() == Some(format!("{}-0", owner.name).as_str()) =>
        {
            Ok(())
        }
        (None, None) => Ok(()),
        _ => Err(conflict("Agent Pod belongs to a different controller")),
    }
}

fn app_id_hint(metadata: &ObjectMeta) -> Option<String> {
    metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get("rcoder.io/app-id"))
        .cloned()
}

fn consistent_app_id(mut hints: std::collections::BTreeSet<Option<String>>) -> Option<String> {
    if hints.len() == 1 {
        hints.pop_first().flatten()
    } else {
        None
    }
}

fn empty(namespace: &str, identifier: &str, service_type: ServiceType) -> AgentStopSnapshot {
    AgentStopSnapshot {
        namespace: Some(namespace.into()),
        identifier: identifier.into(),
        service_type: service_type.family_representative(),
        app_id_hint: None,
        controller: None,
        pods: Vec::new(),
        container: None,
        services: Vec::new(),
        replicas: None,
        endpoint: None,
    }
}

fn validate_snapshot(runtime: &KubernetesRuntime, snapshot: &AgentStopSnapshot) -> Result<()> {
    if snapshot.namespace.as_deref() != Some(runtime.namespace.as_str())
        || snapshot.identifier.is_empty()
        || snapshot.container.is_some()
    {
        return Err(conflict("Agent retirement backend or scope differs"));
    }
    match snapshot.service_type {
        ServiceType::Userapp => {
            return Err(conflict(
                "Production UserApp is not an Agent retirement target",
            ));
        }
        ServiceType::WebAgentRunner
        | ServiceType::ComputerAgentRunner
        | ServiceType::ComputerNormalProject
        | ServiceType::UserappBuilder => {}
    }
    for resource in snapshot
        .controller
        .iter()
        .chain(&snapshot.pods)
        .chain(&snapshot.services)
    {
        if resource.uid.is_empty()
            || resource.name.is_empty()
            || resource
                .resource_version
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Err(conflict("Captured Agent resource identity is incomplete"));
        }
    }
    Ok(())
}

impl KubernetesRuntime {
    pub(super) async fn agent_retirement_inventory(&self) -> Result<Vec<AgentStopSnapshot>> {
        self.agent_retirement_inventory_scope(None).await
    }

    async fn agent_retirement_inventory_scope(
        &self,
        filter: Option<(&str, ServiceType)>,
    ) -> Result<Vec<AgentStopSnapshot>> {
        let lp = ListParams::default().labels(RUNTIME_MANAGED_LABEL);
        let controllers = self.statefulsets().list(&lp).await.map_err(api_error)?;
        require_complete_list(&controllers.metadata, "StatefulSet")?;
        let pods = self.pods().list(&lp).await.map_err(api_error)?;
        require_complete_list(&pods.metadata, "Pod")?;
        let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        let services = services.list(&lp).await.map_err(api_error)?;
        require_complete_list(&services.metadata, "Service")?;
        let mut groups = BTreeMap::<(String, String), AgentStopSnapshot>::new();
        let mut app_hints =
            BTreeMap::<(String, String), std::collections::BTreeSet<Option<String>>>::new();
        for controller in controllers.items {
            if !include(&controller.metadata, filter)? {
                continue;
            }
            if let Some((family, id)) = scope(&controller.metadata) {
                app_hints
                    .entry((family.container_family_key().into(), id.clone()))
                    .or_default()
                    .insert(app_id_hint(&controller.metadata));
                let group = groups
                    .entry((family.container_family_key().into(), id.clone()))
                    .or_insert_with(|| empty(&self.namespace, &id, family));
                if group.controller.is_some() {
                    return Err(conflict(
                        "Multiple managed Agent controllers share one scope",
                    ));
                }
                group.replicas = controller
                    .spec
                    .as_ref()
                    .map(|spec| spec.replicas.unwrap_or(1));
                group.controller = Some(identity(Kind::StatefulSet, &controller.metadata)?);
            }
        }
        for pod in pods.items {
            if !include(&pod.metadata, filter)? {
                continue;
            }
            if let Some((family, id)) = scope(&pod.metadata) {
                app_hints
                    .entry((family.container_family_key().into(), id.clone()))
                    .or_default()
                    .insert(app_id_hint(&pod.metadata));
                let group = groups
                    .entry((family.container_family_key().into(), id.clone()))
                    .or_insert_with(|| empty(&self.namespace, &id, family));
                let canonical_name = super::k8s_pod::K8sPodOps::pod_name(self, &id, &family)?;
                validate_pod_controller(group, &pod, &canonical_name)?;
                if let Some(ip) = pod
                    .status
                    .as_ref()
                    .and_then(|status| status.pod_ip.as_ref())
                {
                    group.endpoint =
                        Some(format!("http://{ip}:{}", shared_types::GRPC_DEFAULT_PORT));
                }
                group.pods.push(identity(Kind::Pod, &pod.metadata)?);
            }
        }
        for service in services.items {
            if !include(&service.metadata, filter)? {
                continue;
            }
            if let Some((family, id)) = scope(&service.metadata) {
                app_hints
                    .entry((family.container_family_key().into(), id.clone()))
                    .or_default()
                    .insert(app_id_hint(&service.metadata));
                let group = groups
                    .entry((family.container_family_key().into(), id.clone()))
                    .or_insert_with(|| empty(&self.namespace, &id, family));
                group
                    .services
                    .push(identity(Kind::Service, &service.metadata)?);
            }
        }
        for (scope, group) in &mut groups {
            if let Some(hints) = app_hints.remove(scope) {
                group.app_id_hint = consistent_app_id(hints);
            }
        }
        Ok(groups.into_values().collect())
    }

    pub(super) async fn observe_agent_compute_absence(
        &self,
        identifier: &str,
        family: &ServiceType,
        registered: &shared_types::ContainerBasicInfo,
    ) -> Result<bool> {
        let name = super::k8s_pod::K8sPodOps::pod_name(self, identifier, family)?;
        if name != registered.container_name {
            return Err(conflict(
                "Registered Agent name differs from its runtime scope",
            ));
        }
        if self
            .capture_agent_retirement(identifier, family)
            .await?
            .has_compute()
        {
            return Ok(false);
        }
        // Also inspect unlabeled/bare Pods and old controller-owned Pods. A
        // damaged label must not turn surviving execution into missing metadata.
        let pods = self
            .pods()
            .list(&ListParams::default())
            .await
            .map_err(api_error)?;
        require_complete_list(&pods.metadata, "Pod")?;
        if pods.items.iter().any(|pod| {
            pod.metadata.uid.as_deref() == Some(registered.container_id.as_str())
                || pod.metadata.name.as_deref() == Some(name.as_str())
                || pod.metadata.name.as_deref() == Some(format!("{name}-0").as_str())
                || pod.metadata.owner_references.iter().flatten().any(|owner| {
                    (owner.kind == "StatefulSet" && owner.name == name)
                        || registered.workload_uid.as_deref() == Some(owner.uid.as_str())
                })
        }) {
            return Ok(false);
        }
        // Recheck after the complete Pod list: a rebuilding controller is not
        // absent even if no Pod was visible during that list.
        Ok(self
            .statefulsets()
            .get_opt(&name)
            .await
            .map_err(api_error)?
            .is_none())
    }

    pub(super) async fn capture_agent_retirement(
        &self,
        identifier: &str,
        family: &ServiceType,
    ) -> Result<AgentStopSnapshot> {
        let mut snapshot = self
            .agent_retirement_inventory_scope(Some((identifier, *family)))
            .await?
            .into_iter()
            .find(|snapshot| {
                snapshot.identifier == identifier
                    && snapshot.service_type.same_container_family(*family)
            })
            .unwrap_or_else(|| empty(&self.namespace, identifier, *family));
        validate_snapshot(self, &snapshot)?;
        let mut app_hints = std::collections::BTreeSet::new();
        if snapshot.controller.is_some()
            || !snapshot.pods.is_empty()
            || !snapshot.services.is_empty()
        {
            app_hints.insert(snapshot.app_id_hint.clone());
        }
        // A known name outside managed discovery is not absence or permission
        // to stop an unowned workload. Historical records are only hints.
        let name = super::k8s_pod::K8sPodOps::pod_name(self, identifier, family)?;
        if let Some(controller) = self
            .statefulsets()
            .get_opt(&name)
            .await
            .map_err(api_error)?
        {
            app_hints.insert(app_id_hint(&controller.metadata));
            if !expected_scope(&controller.metadata, &self.namespace, identifier, *family) {
                return Err(conflict("Agent canonical controller ownership differs"));
            }
            let captured = identity(Kind::StatefulSet, &controller.metadata)?;
            if snapshot
                .controller
                .as_ref()
                .is_some_and(|previous| !previous.same_resource(&captured))
            {
                return Err(conflict("Agent controller changed during capture"));
            }
            snapshot.replicas = controller
                .spec
                .as_ref()
                .map(|spec| spec.replicas.unwrap_or(1));
            snapshot.controller = Some(captured);
        }
        for pod_name in [name.clone(), format!("{name}-0")] {
            if let Some(pod) = self.pods().get_opt(&pod_name).await.map_err(api_error)? {
                app_hints.insert(app_id_hint(&pod.metadata));
                if !expected_scope(&pod.metadata, &self.namespace, identifier, *family) {
                    return Err(conflict("Agent canonical Pod ownership differs"));
                }
                validate_pod_controller(&snapshot, &pod, &name)?;
                let captured = identity(Kind::Pod, &pod.metadata)?;
                if let Some(previous) = snapshot
                    .pods
                    .iter_mut()
                    .find(|previous| previous.name == pod_name)
                {
                    if !previous.same_resource(&captured) {
                        return Err(conflict("Agent Pod changed during capture"));
                    }
                    *previous = captured;
                } else {
                    snapshot.pods.push(captured);
                }
            }
        }
        let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        for suffix in ["svc", "headless"] {
            if let Some(service) = services
                .get_opt(&format!("{name}-{suffix}"))
                .await
                .map_err(api_error)?
            {
                app_hints.insert(app_id_hint(&service.metadata));
                if !expected_scope(&service.metadata, &self.namespace, identifier, *family) {
                    return Err(conflict("Agent canonical Service ownership differs"));
                }
                let captured = identity(Kind::Service, &service.metadata)?;
                if let Some(previous) = snapshot
                    .services
                    .iter_mut()
                    .find(|previous| previous.name == captured.name)
                {
                    if !previous.same_resource(&captured) {
                        return Err(conflict("Agent Service changed during capture"));
                    }
                    *previous = captured;
                } else {
                    snapshot.services.push(captured);
                }
            }
        }
        snapshot.app_id_hint = consistent_app_id(app_hints);
        Ok(snapshot)
    }

    pub(super) async fn retire_agent_snapshot(
        &self,
        snapshot: &AgentStopSnapshot,
        remnants_only: bool,
    ) -> Result<AgentStopOutcome> {
        validate_snapshot(self, snapshot)?;
        if remnants_only && !snapshot.is_remnant() {
            return Err(conflict(
                "Automatic Agent retirement requires a stopped remnant",
            ));
        }
        let deadline = Instant::now() + RETIREMENT_BUDGET;
        tokio::time::timeout_at(deadline, self.protect_agent_volumes(snapshot))
            .await
            .map_err(|_| {
                Error::Timeout("Agent volume safety observation timed out before retirement".into())
            })??;
        // Recheck current presence before an automatic write, without replacing
        // any of the original deletion versions or rediscovering targets.
        if remnants_only {
            let current = self
                .capture_agent_retirement(&snapshot.identifier, &snapshot.service_type)
                .await?;
            let controller_matches = current
                .controller
                .as_ref()
                .map(|item| (&item.uid, &item.resource_version))
                == snapshot
                    .controller
                    .as_ref()
                    .map(|item| (&item.uid, &item.resource_version));
            let terminating_controller = if !controller_matches {
                match (&snapshot.controller, &current.controller) {
                    (Some(target), Some(observed))
                        if target.name == observed.name && target.uid == observed.uid =>
                    {
                        self.statefulsets()
                            .get_opt(&target.name)
                            .await
                            .map_err(api_error)?
                            .is_some_and(|resource| {
                                already_terminating(snapshot, target, &resource.metadata)
                            })
                    }
                    _ => false,
                }
            } else {
                false
            };
            if !current.pods.is_empty()
                || (!controller_matches && !terminating_controller)
                || (current.controller.is_some() && current.replicas != Some(0))
            {
                return Err(conflict("Agent remnant changed before retirement"));
            }
            // A Pod with damaged labels is still execution, not absence. A
            // namespace-wide owner/name check also covers old bare Pods.
            let name = super::k8s_pod::K8sPodOps::pod_name(
                self,
                &snapshot.identifier,
                &snapshot.service_type,
            )?;
            let pods = self
                .pods()
                .list(&ListParams::default())
                .await
                .map_err(api_error)?;
            require_complete_list(&pods.metadata, "Pod")?;
            if pods.items.iter().any(|pod| {
                pod.metadata.name.as_deref() == Some(name.as_str())
                    || pod.metadata.name.as_deref() == Some(format!("{name}-0").as_str())
                    || pod
                        .metadata
                        .owner_references
                        .as_ref()
                        .is_some_and(|owners| {
                            owners.iter().any(|owner| {
                                snapshot
                                    .controller
                                    .as_ref()
                                    .is_some_and(|controller| owner.uid == controller.uid)
                            })
                        })
            }) {
                return Err(conflict("Agent remnant still has a physical Pod"));
            }
            // A Service's scope labels do not prove that its selected execution
            // is absent. Its selector may still route to a Pod with damaged
            // scope labels, a foreign name, or an unrelated controller. Reuse
            // the complete Pod list above, never only the managed inventory.
            let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
            for captured in &snapshot.services {
                let Some(service) = services.get_opt(&captured.name).await.map_err(api_error)?
                else {
                    continue;
                };
                if (identity(Kind::Service, &service.metadata)? != *captured
                    && !already_terminating(snapshot, captured, &service.metadata))
                    || !expected_scope(
                        &service.metadata,
                        &self.namespace,
                        &snapshot.identifier,
                        snapshot.service_type,
                    )
                {
                    return Err(conflict("Agent remnant Service changed before retirement"));
                }
                let spec = service
                    .spec
                    .as_ref()
                    .ok_or_else(|| conflict("Agent remnant Service routing is unknown"))?;
                if spec.external_name.is_some() || spec.type_.as_deref() == Some("ExternalName") {
                    return Err(conflict(
                        "ExternalName Service is not an automatic Agent retirement target",
                    ));
                }
                let selector = spec
                    .selector
                    .as_ref()
                    .filter(|selector| !selector.is_empty())
                    .ok_or_else(|| {
                        conflict("Agent remnant Service has no observable Pod selector")
                    })?;
                if pods.items.iter().any(|pod| {
                    pod.metadata.labels.as_ref().is_some_and(|labels| {
                        selector
                            .iter()
                            .all(|(key, value)| labels.get(key) == Some(value))
                    })
                }) {
                    return Err(conflict(
                        "Agent remnant Service still selects physical execution",
                    ));
                }
            }
        }
        tokio::time::timeout_at(deadline, async {
            if let Some(controller) = &snapshot.controller {
                self.delete_agent_resource::<StatefulSet>(snapshot, controller, deadline)
                    .await?;
            }
            for pod in &snapshot.pods {
                // Foreground controller deletion normally removed these already.
                // Orphan/bare Pods still use their own immutable UID/version.
                self.delete_agent_resource::<Pod>(snapshot, pod, deadline)
                    .await?;
            }
            Ok::<_, Error>(())
        })
        .await
        .map_err(|_| Error::Timeout("Captured Agent compute exit is unconfirmed".into()))??;
        let mut pending = Vec::new();
        for service in &snapshot.services {
            let result = tokio::time::timeout_at(
                deadline,
                self.delete_agent_resource::<Service>(snapshot, service, deadline),
            )
            .await;
            let error = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(_) => Some("Captured Service cleanup deadline exceeded".into()),
            };
            if let Some(error) = error {
                pending.push(AgentServiceCleanupFailure {
                    name: service.name.clone(),
                    uid: service.uid.clone(),
                    error,
                });
            }
        }
        let mut cache = self.pod_cache.write().await;
        if cache.get(&snapshot.identifier).is_some_and(|entry| {
            snapshot
                .pods
                .iter()
                .any(|pod| pod.uid == entry.info.container_id)
        }) {
            cache.remove(&snapshot.identifier);
        }
        Ok(if pending.is_empty() {
            AgentStopOutcome::Complete
        } else {
            AgentStopOutcome::ComputeGoneServicesPending { pending }
        })
    }

    async fn protect_agent_volumes(&self, snapshot: &AgentStopSnapshot) -> Result<()> {
        if let Some(target) = &snapshot.controller
            && let Some(current) = self
                .statefulsets()
                .get_opt(&target.name)
                .await
                .map_err(api_error)?
        {
            if current.metadata.uid.as_deref() != Some(target.uid.as_str())
                || (current.metadata.resource_version != target.resource_version
                    && !already_terminating(snapshot, target, &current.metadata))
            {
                return Err(conflict(
                    "Agent controller changed before volume safety observation",
                ));
            }
            if let Some(spec) = &current.spec
                && spec
                    .volume_claim_templates
                    .as_ref()
                    .is_some_and(|templates| !templates.is_empty())
                && spec
                    .persistent_volume_claim_retention_policy
                    .as_ref()
                    .and_then(|policy| policy.when_deleted.as_deref())
                    == Some("Delete")
            {
                return Err(conflict(
                    "Agent controller deletion would delete volume claims; compute and data are retained",
                ));
            }
        }
        let targets: std::collections::BTreeSet<&str> = snapshot
            .controller
            .iter()
            .chain(&snapshot.pods)
            .chain(&snapshot.services)
            .map(|resource| resource.uid.as_str())
            .collect();
        if targets.is_empty() {
            return Ok(());
        }
        // Kubernetes GC follows PVC ownerReferences independently of the
        // runtime's DELETE target list. Do not rewrite the user's ownership or
        // retention policy to manufacture a successful compute retirement.
        let api: Api<PersistentVolumeClaim> = Api::namespaced(self.client.clone(), &self.namespace);
        let claims = api.list(&ListParams::default()).await.map_err(api_error)?;
        require_complete_list(&claims.metadata, "PersistentVolumeClaim")?;
        if claims.items.iter().any(|claim| {
            claim
                .metadata
                .owner_references
                .iter()
                .flatten()
                .any(|owner| targets.contains(owner.uid.as_str()))
        }) {
            return Err(conflict(
                "Agent retirement would garbage-collect an owned PVC; compute and data are retained",
            ));
        }
        Ok(())
    }

    async fn delete_agent_resource<K>(
        &self,
        snapshot: &AgentStopSnapshot,
        target: &Identity,
        deadline: Instant,
    ) -> Result<()>
    where
        K: Clone
            + Debug
            + DeserializeOwned
            + Send
            + 'static
            + Resource<DynamicType = (), Scope = k8s_openapi::NamespaceResourceScope>,
    {
        let api: Api<K> = Api::namespaced(self.client.clone(), &self.namespace);
        let Some(current) = api.get_opt(&target.name).await.map_err(api_error)? else {
            return Ok(());
        };
        let meta = current.meta();
        if meta.uid.as_deref() != Some(&target.uid) {
            return Err(conflict("Captured Agent resource was replaced"));
        }
        if !expected_scope(
            meta,
            &self.namespace,
            &snapshot.identifier,
            snapshot.service_type,
        ) {
            return Err(conflict("Captured Agent resource ownership changed"));
        }
        if !already_terminating(snapshot, target, meta) {
            if meta.resource_version != target.resource_version {
                return Err(conflict("Captured Agent resource version changed"));
            }
            let params = DeleteParams {
                preconditions: Some(Preconditions {
                    uid: Some(target.uid.clone()),
                    resource_version: target.resource_version.clone(),
                }),
                propagation_policy: Some(kube::api::PropagationPolicy::Foreground),
                ..Default::default()
            };
            match api.delete(&target.name, &params).await {
                Ok(_) => {}
                Err(kube::Error::Api(error)) if error.code == 404 => {}
                Err(error) => return Err(api_error(error)),
            }
        }
        // DELETE may already have committed before an ACK/observation failure.
        // Never resend it or refresh the original RV for a terminating UID.
        tokio::time::timeout_at(
            deadline,
            await_condition(api, &target.name, conditions::is_deleted(&target.uid)),
        )
        .await
        .map_err(|_| Error::Timeout("Captured Agent resource remains present".into()))?
        .map_err(|error| Error::K8sError(format!("Observe captured Agent exit: {error}")))?;
        Ok(())
    }

    pub(super) async fn fence_agent_snapshot(
        &self,
        snapshot: &AgentStopSnapshot,
    ) -> Result<AgentStopSnapshot> {
        validate_snapshot(self, snapshot)?;
        let mut updated = snapshot.clone();
        if let Some(controller) = &mut updated.controller {
            self.fence_agent_resource::<StatefulSet>(snapshot, controller)
                .await?;
        }
        for pod in &mut updated.pods {
            self.fence_agent_resource::<Pod>(snapshot, pod).await?;
        }
        for service in &mut updated.services {
            self.fence_agent_resource::<Service>(snapshot, service)
                .await?;
        }
        Ok(updated)
    }

    async fn fence_agent_resource<K>(
        &self,
        snapshot: &AgentStopSnapshot,
        target: &mut Identity,
    ) -> Result<()>
    where
        K: Clone
            + Debug
            + DeserializeOwned
            + Serialize
            + Resource<DynamicType = (), Scope = k8s_openapi::NamespaceResourceScope>,
    {
        let api: Api<K> = Api::namespaced(self.client.clone(), &self.namespace);
        // Admission may observe a newer status RV while retaining the exact UID.
        // Only this fencing write may recapture an RV; DELETE never does so.
        for attempt in 0..3 {
            let current = api.get(&target.name).await.map_err(api_error)?;
            let meta = current.meta();
            if meta.uid.as_deref() != Some(&target.uid)
                || meta.deletion_timestamp.is_some()
                || !expected_scope(
                    meta,
                    &self.namespace,
                    &snapshot.identifier,
                    snapshot.service_type,
                )
            {
                return Err(conflict("Agent changed before admission fence"));
            }
            let patch = serde_json::json!({"metadata": {"uid": target.uid,
                "resourceVersion": meta.resource_version,
                "annotations": {FENCE_ANNOTATION: uuid::Uuid::new_v4().to_string()}}});
            match api
                .patch(&target.name, &PatchParams::default(), &Patch::Merge(patch))
                .await
            {
                Ok(patched) => {
                    *target = identity(target.kind, patched.meta())?;
                    return Ok(());
                }
                Err(kube::Error::Api(error)) if error.code == 409 => {
                    if attempt == 2 {
                        return Err(conflict("Agent admission fence changed repeatedly"));
                    }
                }
                Err(error) => return Err(api_error(error)),
            }
        }
        Err(conflict("Agent admission fence changed repeatedly"))
    }
}

fn include(metadata: &ObjectMeta, filter: Option<(&str, ServiceType)>) -> Result<bool> {
    if let Some((family, identifier)) = scope(metadata) {
        return Ok(filter.is_none_or(|(expected_id, expected_family)| {
            expected_id == identifier && family.same_container_family(expected_family)
        }));
    }
    if filter.is_none() {
        return Err(conflict(
            "Managed Agent resource ownership is unreadable; inventory is incomplete",
        ));
    }
    // Single-scope operations do not inherit a malformed unrelated resource.
    // Their canonical names are independently inspected by capture.
    Ok(false)
}
