use super::*;
use shared_types::{
    ComputeLeaseInspection, PreparedComputeLease, UserAppExecutionContext,
    UserAppOperationLeaseReceipt, UserAppOperationScope,
};

/// Ordinary operation metadata identifies a prior business owner, not this
/// compute's prepared attempt. Discovery is read-only; durable settlement is
/// still the coordinator's responsibility.
fn previous_owner_context(
    lease: &Lease,
    current: &UserAppExecutionContext,
    family: &ServiceType,
) -> Result<UserAppExecutionContext, String> {
    let labels = lease.metadata.labels.as_ref();
    if labels.and_then(|values| values.get("rcoder.io/operation-app")) != Some(&current.app_id)
        || labels.and_then(|values| values.get("rcoder.io/operation-family"))
            != Some(&family.to_string())
    {
        return Err("Previous operation lease ownership differs".into());
    }
    let annotations = lease
        .metadata
        .annotations
        .as_ref()
        .ok_or_else(|| "Previous operation lease identity is missing".to_string())?;
    if annotations.contains_key("rcoder.io/compute-lease")
        || annotations.contains_key("rcoder.io/lease-token")
    {
        return Err("Previous operation lease is not an ordinary owner".into());
    }
    let required = |key: &str| {
        annotations
            .get(key)
            .cloned()
            .ok_or_else(|| format!("Previous operation lease identity is missing: {key}"))
    };
    let owner = UserAppExecutionContext {
        app_id: current.app_id.clone(),
        lifecycle_id: required("rcoder.io/lifecycle-id")?,
        operation_id: required("rcoder.io/operation-id")?,
        executor_id: required("rcoder.io/executor-id")?,
        request_fingerprint: required("rcoder.io/request-fingerprint")?,
    };
    owner.validate_identity(&current.app_id)?;
    if owner.lifecycle_id != current.lifecycle_id || owner.operation_id == current.operation_id {
        return Err("Previous operation lease lifecycle or operation differs".into());
    }
    let holder = format!("{}:{}", owner.executor_id, owner.operation_id);
    if lease
        .spec
        .as_ref()
        .and_then(|spec| spec.holder_identity.as_deref())
        != Some(holder.as_str())
        || holder_token(lease).as_deref() != Some(owner.operation_id.as_str())
    {
        return Err("Previous operation lease holder differs".into());
    }
    Ok(owner)
}

struct PreparedLease(Box<dyn AppOperationLease>);
#[async_trait::async_trait]
impl AppOperationLease for PreparedLease {
    fn receipt(&self) -> Option<UserAppOperationLeaseReceipt> {
        self.0.receipt()
    }
    async fn release(self: Box<Self>) -> Result<(), String> {
        self.0.release().await
    }
}
#[async_trait::async_trait]
impl PreparedComputeLease for PreparedLease {
    async fn activate(&mut self) -> Result<(), String> {
        Ok(())
    }
}

impl KubernetesRuntime {
    pub(crate) async fn prepare_compute_k8s_lease(
        &self,
        context: &UserAppExecutionContext,
        scope: UserAppOperationScope,
    ) -> ContainerRuntimeResult<Box<dyn PreparedComputeLease>> {
        let family = shared_types::compute_lease_family(scope)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        Ok(Box::new(PreparedLease(
            self.acquire_operation_lease(&context.app_id, &family, Some(context), true)
                .await?,
        )))
    }

    pub(crate) async fn inspect_compute_k8s_lease(
        &self,
        context: &UserAppExecutionContext,
        scope: UserAppOperationScope,
        receipt: Option<&UserAppOperationLeaseReceipt>,
    ) -> ContainerRuntimeResult<ComputeLeaseInspection> {
        use ComputeLeaseInspection::*;
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let family = shared_types::compute_lease_family(scope)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let name = operation_name(&context.app_id, &family)?;
        if let Some(receipt) = receipt {
            receipt
                .validate()
                .map_err(ContainerRuntimeError::ConfigurationError)?;
            let UserAppOperationLeaseReceipt::Kubernetes {
                service_type,
                namespace,
                name: captured_name,
                ..
            } = receipt
            else {
                return Ok(IdentityChanged("Compute lease runtime differs".into()));
            };
            if *service_type != family || namespace != &self.namespace || captured_name != &name {
                return Ok(IdentityChanged("Compute lease scope differs".into()));
            }
        }
        let api: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
        let current = api.get_opt(&name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Inspect compute lease: {error}"))
        })?;
        let Some(current) = current else {
            // Pre-Lease stock has no attempt metadata: recover only a bound,
            // exactly matching receipt, never a same-name ConfigMap alone.
            let legacy: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
            let Some(object) = legacy.get_opt(&name).await.map_err(|error| {
                ContainerRuntimeError::K8sError(format!("Inspect legacy compute lease: {error}"))
            })?
            else {
                return Ok(Absent);
            };
            if let Some(UserAppOperationLeaseReceipt::Kubernetes {
                uid,
                resource_version,
                token,
                ..
            }) = receipt
                && configmap_identity_matches(
                    &object,
                    context,
                    &family,
                    uid,
                    resource_version,
                    token,
                )
            {
                return Ok(Releasable(receipt.cloned().ok_or_else(|| {
                    ContainerRuntimeError::ConfigurationError("Legacy receipt missing".into())
                })?));
            }
            return Ok(IdentityChanged(
                "Legacy compute lease lacks an exact registered receipt".into(),
            ));
        };
        let label = |key: &str| {
            current
                .metadata
                .labels
                .as_ref()
                .and_then(|values| values.get(key))
                .map(String::as_str)
        };
        if label("rcoder.io/operation-app") != Some(context.app_id.as_str())
            || label("rcoder.io/operation-family") != Some(family.to_string().as_str())
        {
            return Ok(IdentityChanged("Compute lease ownership differs".into()));
        }
        let token = holder_token(&current);
        if let Some(UserAppOperationLeaseReceipt::Kubernetes {
            uid,
            token: captured_token,
            ..
        }) = receipt
        {
            if current.metadata.uid.as_deref() != Some(uid)
                || token.as_deref() != Some(captured_token)
            {
                return Ok(IdentityChanged(
                    "Captured compute lease identity changed".into(),
                ));
            }
            // A bound old-format receipt remains valid; early-stage durable CAS
            // has revoked execution, so expiry is not used as write evidence.
            return Ok(Releasable(receipt.cloned().ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError("Captured receipt missing".into())
            })?));
        }
        let annotation = |key: &str| {
            current
                .metadata
                .annotations
                .as_ref()
                .and_then(|values| values.get(key))
                .map(String::as_str)
        };
        if annotation("rcoder.io/compute-lease").is_none()
            && annotation("rcoder.io/lease-token").is_none()
        {
            let owner = match previous_owner_context(&current, context, &family) {
                Ok(owner) => owner,
                Err(reason) => return Ok(IdentityChanged(reason)),
            };
            let (Some(uid), Some(resource_version)) = (
                current
                    .metadata
                    .uid
                    .clone()
                    .filter(|value| !value.is_empty()),
                current
                    .metadata
                    .resource_version
                    .clone()
                    .filter(|value| !value.is_empty()),
            ) else {
                return Ok(IdentityChanged(
                    "Previous operation lease UID or resourceVersion missing".into(),
                ));
            };
            if !lease_expired_at(k8s_openapi::jiff::Timestamp::now(), &current) {
                return Ok(Held);
            }
            return Ok(PreviousOwner {
                receipt: UserAppOperationLeaseReceipt::Kubernetes {
                    service_type: family,
                    namespace: self.namespace.clone(),
                    name,
                    uid,
                    resource_version,
                    token: owner.operation_id.clone(),
                },
                owner,
            });
        }
        if annotation("rcoder.io/compute-lease") != Some("true")
            || annotation("rcoder.io/operation-id") != Some(context.operation_id.as_str())
            || annotation("rcoder.io/lifecycle-id") != Some(context.lifecycle_id.as_str())
            || annotation("rcoder.io/request-fingerprint")
                != Some(context.request_fingerprint.as_str())
            || annotation("rcoder.io/lease-token") != token.as_deref()
            || token.as_deref() != annotation("rcoder.io/executor-id")
        {
            return Ok(IdentityChanged(
                "Unregistered compute lease lacks this executor's attempt identity".into(),
            ));
        }
        // Every attempt of this original operation was fenced while still
        // draining. Also identify a late create from its prior prepared attempt;
        // never infer permission from TTL or from a same-name foreign lock.
        let mut attempt = context.clone();
        attempt.executor_id = token.ok_or_else(|| {
            ContainerRuntimeError::K8sError("Compute attempt token missing".into())
        })?;
        attempt
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let uid = current
            .metadata
            .uid
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ContainerRuntimeError::K8sError("Compute lease UID missing".into()))?;
        let resource_version = current
            .metadata
            .resource_version
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ContainerRuntimeError::K8sError("Compute lease resourceVersion missing".into())
            })?;
        Ok(Discovered {
            receipt: UserAppOperationLeaseReceipt::Kubernetes {
                service_type: family,
                namespace: self.namespace.clone(),
                name,
                uid,
                resource_version,
                token: attempt.executor_id.clone(),
            },
            attempt,
        })
    }

    /// Release an observed ordinary predecessor after the coordinator has
    /// proved its exact durable owner terminal and fenced. Unlike completion
    /// cleanup, a changed revision must be observed again before any release.
    pub(crate) async fn release_previous_compute_k8s_lease(
        &self,
        current: &UserAppExecutionContext,
        scope: UserAppOperationScope,
        owner: &UserAppExecutionContext,
        receipt: &UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<()> {
        current
            .validate_identity(&current.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        owner
            .validate_identity(&current.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        receipt
            .validate()
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let family = shared_types::compute_lease_family(scope)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let expected_name = operation_name(&current.app_id, &family)?;
        let UserAppOperationLeaseReceipt::Kubernetes {
            service_type,
            namespace,
            name,
            uid,
            resource_version,
            token,
        } = receipt
        else {
            return Err(ContainerRuntimeError::Conflict(
                "Previous operation lease runtime differs".into(),
            ));
        };
        if *service_type != family
            || namespace != &self.namespace
            || name != &expected_name
            || owner.lifecycle_id != current.lifecycle_id
            || owner.operation_id == current.operation_id
            || token != &owner.operation_id
        {
            return Err(ContainerRuntimeError::Conflict(
                "Previous operation lease scope or owner differs".into(),
            ));
        }
        let api: Api<Lease> = Api::namespaced(self.client.clone(), namespace);
        let Some(observed) = api.get_opt(name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Inspect previous operation lease: {error}"))
        })?
        else {
            return Ok(());
        };
        let observed_owner = previous_owner_context(&observed, current, &family)
            .map_err(ContainerRuntimeError::Conflict)?;
        if observed.metadata.uid.as_deref() != Some(uid)
            || observed.metadata.resource_version.as_deref() != Some(resource_version)
            || holder_token(&observed).as_deref() != Some(token)
            || &observed_owner != owner
            || !lease_expired_at(k8s_openapi::jiff::Timestamp::now(), &observed)
        {
            return Err(ContainerRuntimeError::Conflict(
                "Previous operation lease changed after inspection".into(),
            ));
        }
        let params = DeleteParams {
            preconditions: Some(Preconditions {
                uid: Some(uid.clone()),
                resource_version: Some(resource_version.clone()),
            }),
            ..Default::default()
        };
        match api.delete(name, &params).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(error)) if error.code == 404 => Ok(()),
            Err(kube::Error::Api(error)) if error.code == 409 => {
                Err(ContainerRuntimeError::Conflict(
                    "Previous operation lease changed during conditional release".into(),
                ))
            }
            Err(error) => Err(ContainerRuntimeError::K8sError(format!(
                "Release previous operation lease: {error}"
            ))),
        }
    }
}
