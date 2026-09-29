use super::*;
use shared_types::{
    ComputeLeaseInspection, PreparedComputeLease, UserAppExecutionContext,
    UserAppOperationLeaseReceipt, UserAppOperationScope,
};

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
}
