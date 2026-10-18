//! Pre-fence liveness uses real renewal and a conditional retained mutex.
//! It does not use the post-fence inspection contract as a death proof.
use super::*;
use container_runtime_api::ComputeHandoff;

struct RenewalHandoff(tokio_util::sync::CancellationToken);
impl container_runtime_api::ComputeHandoffGuard for RenewalHandoff {}
impl Drop for RenewalHandoff {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl KubernetesRuntime {
    pub(crate) async fn prepare_compute_k8s_handoff(
        &self,
        context: &UserAppExecutionContext,
        scope: UserAppOperationScope,
        receipt: Option<&UserAppOperationLeaseReceipt>,
    ) -> ContainerRuntimeResult<ComputeHandoff> {
        use ComputeLeaseInspection::IdentityChanged;
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
                namespace,
                name: captured_name,
                service_type,
                ..
            } = receipt
            else {
                return Ok(ComputeHandoff::deferred(IdentityChanged(
                    "Compute handoff runtime differs".into(),
                )));
            };
            if namespace != &self.namespace || captured_name != &name || *service_type != family {
                return Ok(ComputeHandoff::deferred(IdentityChanged(
                    "Compute handoff scope differs".into(),
                )));
            }
        }
        let api: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
        let Some(current) = api.get_opt(&name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Observe compute handoff: {error}"))
        })?
        else {
            // A legacy exact ConfigMap has no renewable native Lease. Keep its
            // receipt for the existing post-CAS cleanup instead of manufacturing
            // a new native UID that contradicts that historical receipt.
            let legacy: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
            if let Some(object) = legacy.get_opt(&name).await.map_err(|error| {
                ContainerRuntimeError::K8sError(format!("Observe legacy compute handoff: {error}"))
            })? {
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
                    return Ok(ComputeHandoff::Vacant {
                        original_absent: false,
                    });
                }
                return Ok(ComputeHandoff::deferred(IdentityChanged(
                    "Legacy compute handoff ownership is unconfirmed".into(),
                )));
            }
            return Ok(ComputeHandoff::Vacant {
                original_absent: true,
            });
        };
        let labels = current.metadata.labels.as_ref();
        if labels.and_then(|values| values.get("rcoder.io/operation-app")) != Some(&context.app_id)
            || labels.and_then(|values| values.get("rcoder.io/operation-family"))
                != Some(&family.to_string())
            || current.metadata.deletion_timestamp.is_some()
        {
            return Ok(ComputeHandoff::deferred(IdentityChanged(
                "Compute handoff ownership changed".into(),
            )));
        }
        let bound = match receipt {
            Some(UserAppOperationLeaseReceipt::Kubernetes { uid, token, .. }) => {
                current.metadata.uid.as_deref() == Some(uid.as_str())
                    && holder_token(&current).as_deref() == Some(token.as_str())
            }
            Some(UserAppOperationLeaseReceipt::Docker { .. }) => {
                return Ok(ComputeHandoff::deferred(IdentityChanged(
                    "Compute handoff runtime differs".into(),
                )));
            }
            None => false,
        };
        let annotations = current.metadata.annotations.as_ref();
        let value = |key: &str| {
            annotations
                .and_then(|values| values.get(key))
                .map(String::as_str)
        };
        if bound {
            let original = receipt.ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(
                    "Bound compute handoff receipt missing".into(),
                )
            })?;
            match self
                .observe_original_application_lease(context, original)
                .await?
            {
                container_runtime_api::AppOperationLeaseObservation::Held => {
                    return Ok(ComputeHandoff::Held);
                }
                container_runtime_api::AppOperationLeaseObservation::ExpiredOriginalIdentity => {}
                container_runtime_api::AppOperationLeaseObservation::Unconfirmed => {
                    return Ok(ComputeHandoff::deferred(IdentityChanged(
                        "Original compute handoff is unconfirmed".into(),
                    )));
                }
            }
        }
        if !bound
            && value("rcoder.io/compute-lease").is_none()
            && value("rcoder.io/lease-token").is_none()
        {
            let owner = match previous_owner_context(&current, context, &family) {
                Ok(owner) => owner,
                Err(reason) => return Ok(ComputeHandoff::deferred(IdentityChanged(reason))),
            };
            if !lease_expired_at(k8s_openapi::jiff::Timestamp::now(), &current) {
                return Ok(ComputeHandoff::Held);
            }
            let observed = captured(&current, &self.namespace, &name, family)?;
            return Ok(ComputeHandoff::deferred(
                ComputeLeaseInspection::PreviousOwner {
                    receipt: observed,
                    owner,
                },
            ));
        }
        if !bound {
            let actual_executor = value("rcoder.io/executor-id");
            let expected_holder =
                actual_executor.map(|executor| format!("{}:{}", executor, context.operation_id));
            if value("rcoder.io/compute-lease") != Some("true")
                || actual_executor.is_none_or(str::is_empty)
                || value("rcoder.io/lease-token") != actual_executor
                || current
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.holder_identity.as_deref())
                    != expected_holder.as_deref()
            {
                return Ok(ComputeHandoff::deferred(IdentityChanged(
                    "Unregistered compute handoff attempt changed".into(),
                )));
            }
        }
        for (key, expected) in [
            ("rcoder.io/operation-id", context.operation_id.as_str()),
            ("rcoder.io/lifecycle-id", context.lifecycle_id.as_str()),
            (
                "rcoder.io/request-fingerprint",
                context.request_fingerprint.as_str(),
            ),
        ] {
            if value(key).is_some_and(|actual| actual != expected)
                || (!bound && value(key).is_none())
            {
                return Ok(ComputeHandoff::deferred(IdentityChanged(
                    "Compute handoff execution identity changed".into(),
                )));
            }
        }
        if !lease_expired_at(k8s_openapi::jiff::Timestamp::now(), &current) {
            return Ok(ComputeHandoff::Held);
        }
        let observed = captured(&current, &self.namespace, &name, family)?;
        if !bound && value("rcoder.io/executor-id") != Some(context.executor_id.as_str()) {
            let mut attempt = context.clone();
            attempt.executor_id = value("rcoder.io/executor-id")
                .ok_or_else(|| {
                    ContainerRuntimeError::ConfigurationError(
                        "Observed compute executor missing".into(),
                    )
                })?
                .into();
            attempt
                .validate_identity(&context.app_id)
                .map_err(ContainerRuntimeError::ConfigurationError)?;
            return Ok(ComputeHandoff::deferred(
                ComputeLeaseInspection::Discovered {
                    receipt: observed,
                    attempt,
                },
            ));
        }
        let UserAppOperationLeaseReceipt::Kubernetes {
            uid,
            resource_version,
            token,
            ..
        } = &observed
        else {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Compute handoff requires a native receipt".into(),
            ));
        };
        // Keep the original token. A late/unknown PATCH can be inspected using
        // the already durable original attempt; no unregistered new identity.
        let mut patch = renewal_patch(resource_version)?;
        patch["metadata"]["uid"] = uid.clone().into();
        match api
            .patch(&name, &PatchParams::default(), &Patch::Merge(patch))
            .await
        {
            Ok(_) => {}
            Err(kube::Error::Api(status)) if status.code == 404 || status.code == 409 => {
                return Ok(ComputeHandoff::Held);
            }
            Err(error) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "Retain exact compute handoff lease: {error}"
                )));
            }
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        spawn_renewal(
            api.clone(),
            name.clone(),
            uid.clone(),
            token.clone(),
            cancel.clone(),
        );
        Ok(ComputeHandoff::Acquired {
            guard: Box::new(RenewalHandoff(cancel)),
            original_absent: receipt.is_some() && !bound,
        })
    }
}

fn captured(
    object: &Lease,
    namespace: &str,
    name: &str,
    family: ServiceType,
) -> ContainerRuntimeResult<UserAppOperationLeaseReceipt> {
    let required = |value: Option<&str>, label: &str| {
        value
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                ContainerRuntimeError::K8sError(format!("Compute handoff {label} missing"))
            })
    };
    Ok(UserAppOperationLeaseReceipt::Kubernetes {
        service_type: family,
        namespace: namespace.into(),
        name: name.into(),
        uid: required(object.metadata.uid.as_deref(), "UID")?,
        resource_version: required(
            object.metadata.resource_version.as_deref(),
            "resourceVersion",
        )?,
        token: holder_token(object).ok_or_else(|| {
            ContainerRuntimeError::K8sError("Compute handoff token missing".into())
        })?,
    })
}
