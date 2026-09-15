//! Per-application operation mutex. No expiration or automatic stale-owner takeover.
//! A crashed owner's ConfigMap requires operator recovery after proving quiescence.
//! Builder callers release explicit request rejections. Transport/unknown failures,
//! panic and cancellation retain the lease until recovery proves quiescence.
use super::kubernetes_runtime::KubernetesRuntime;
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult};
use k8s_openapi::api::core::v1::ConfigMap;
use kube::{
    Api,
    api::{DeleteParams, PostParams, Preconditions},
};
use shared_types::{AppOperationLease, ServiceType};

struct OperationLease {
    api: Api<ConfigMap>,
    name: String,
    uid: String,
    version: String,
    released: bool,
    receipt: shared_types::UserAppOperationLeaseReceipt,
}

async fn remove_owned(
    api: &Api<ConfigMap>,
    name: &str,
    uid: &str,
    version: &str,
) -> Result<(), String> {
    let params = DeleteParams {
        preconditions: Some(Preconditions {
            uid: Some(uid.into()),
            resource_version: Some(version.into()),
        }),
        ..Default::default()
    };
    match api.delete(name, &params).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(()),
        Err(error) => Err(format!("release application operation {name}: {error}")),
    }
}

#[async_trait::async_trait]
impl AppOperationLease for OperationLease {
    fn receipt(&self) -> Option<shared_types::UserAppOperationLeaseReceipt> {
        Some(self.receipt.clone())
    }
    async fn release(mut self: Box<Self>) -> Result<(), String> {
        let result = remove_owned(&self.api, &self.name, &self.uid, &self.version).await;
        self.released = true;
        result
    }
}

impl Drop for OperationLease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        // A cancelled HTTP future may leave an API-server write in flight.
        // Releasing here would allow purge to race that late write. Only the
        // explicit completion path may release; recovery must prove quiescence.
        tracing::error!(name = %self.name, uid = %self.uid,
            "application operation lease retained after incomplete operation; operator recovery required");
    }
}

impl KubernetesRuntime {
    pub(super) async fn release_captured_application_operation(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<()> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        receipt
            .validate()
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        let shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            service_type,
            namespace,
            name,
            uid,
            resource_version,
            token,
        } = receipt
        else {
            return Err(ContainerRuntimeError::ConfigurationError(
                "Kubernetes lease receipt is required".into(),
            ));
        };
        if namespace != &self.namespace || name != &operation_name(&context.app_id, service_type)? {
            return Err(ContainerRuntimeError::Conflict(
                "Operation lease scope changed".into(),
            ));
        }
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), namespace);
        let Some(current) = api.get_opt(name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Observe captured operation lease: {error}"))
        })?
        else {
            return Ok(());
        };
        let annotations = current.metadata.annotations.as_ref();
        let labels = current.metadata.labels.as_ref();
        let observed_token = annotations.and_then(|values| {
            values
                .get("rcoder.io/operation-id")
                .or_else(|| values.get("rcoder.io/legacy-operation-id"))
        });
        if current.metadata.uid.as_deref() != Some(uid.as_str())
            || current.metadata.resource_version.as_deref() != Some(resource_version.as_str())
            || observed_token != Some(token)
            || labels
                .and_then(|values| values.get("rcoder.io/operation-app"))
                .map(String::as_str)
                != Some(context.app_id.as_str())
            || labels
                .and_then(|values| values.get("rcoder.io/operation-family"))
                .map(String::as_str)
                != Some(service_type.to_string().as_str())
        {
            return Err(ContainerRuntimeError::Conflict(
                "Captured operation lease identity changed".into(),
            ));
        }
        remove_owned(&api, name, uid, resource_version)
            .await
            .map_err(ContainerRuntimeError::K8sError)
    }

    pub(super) async fn acquire_application_operation(
        &self,
        app_id: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Box<dyn AppOperationLease>> {
        self.acquire_application_operation_with_context(app_id, service_type, None)
            .await
    }

    pub(super) async fn acquire_application_operation_with_context(
        &self,
        app_id: &str,
        service_type: &ServiceType,
        context: Option<&shared_types::UserAppExecutionContext>,
    ) -> ContainerRuntimeResult<Box<dyn AppOperationLease>> {
        if !matches!(
            service_type,
            ServiceType::Userapp | ServiceType::UserappBuilder
        ) {
            return Err(ContainerRuntimeError::ConfigurationError(
                "application operation lease only supports UserApp families".into(),
            ));
        }
        let name = operation_name(app_id, service_type)?;
        let mut annotations = std::collections::BTreeMap::new();
        if let Some(context) = context {
            context
                .validate_identity(app_id)
                .map_err(ContainerRuntimeError::ConfigurationError)?;
            annotations.insert(
                "rcoder.io/operation-id".into(),
                context.operation_id.clone(),
            );
            annotations.insert(
                "rcoder.io/lifecycle-id".into(),
                context.lifecycle_id.clone(),
            );
            annotations.insert("rcoder.io/executor-id".into(), context.executor_id.clone());
            annotations.insert(
                "rcoder.io/request-fingerprint".into(),
                context.request_fingerprint.clone(),
            );
        } else {
            // Legacy locks are intentionally not advertised as durable operations.
            annotations.insert(
                "rcoder.io/legacy-operation-id".into(),
                uuid::Uuid::new_v4().to_string(),
            );
        }
        let token = annotations
            .get("rcoder.io/operation-id")
            .or_else(|| annotations.get("rcoder.io/legacy-operation-id"))
            .cloned()
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError("Operation token is missing".into())
            })?;
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let object = ConfigMap {
            metadata: kube::api::ObjectMeta {
                name: Some(name.clone()),
                labels: Some(std::collections::BTreeMap::from([
                    (
                        "rcoder.io/operation-family".into(),
                        service_type.to_string(),
                    ),
                    ("rcoder.io/operation-app".into(), app_id.into()),
                ])),
                annotations: Some(annotations),
                ..Default::default()
            },
            ..Default::default()
        };
        let created = match api.create(&PostParams::default(), &object).await {
            Ok(created) => created,
            Err(kube::Error::Api(status)) if status.code == 409 => {
                // Read the winner's identity, never echo this losing request's UUID.
                // A disappearing or legacy lease has no identifiable operation.
                let current = api.get_opt(&name).await.map_err(|error| {
                    ContainerRuntimeError::K8sError(format!(
                        "read application operation {name}: {error}"
                    ))
                })?;
                let operation_id = current
                    .and_then(|current| current.metadata.annotations)
                    .and_then(|annotations| annotations.get("rcoder.io/operation-id").cloned())
                    .filter(|id| !id.is_empty());
                return Err(ContainerRuntimeError::OperationInProgress(Box::new(
                    shared_types::UserAppOperationInProgress {
                        app_id: app_id.into(),
                        service_type: service_type.clone(),
                        resource_name: name,
                        operation_id,
                    },
                )));
            }
            Err(error) => {
                return Err(ContainerRuntimeError::K8sError(format!(
                    "acquire application operation {name}: {error}"
                )));
            }
        };
        let uid = created
            .metadata
            .uid
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(
                    "created operation lease has no UID; operator recovery required".into(),
                )
            })?;
        let version = created
            .metadata
            .resource_version
            .filter(|version| !version.is_empty())
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(
                    "created operation lease has no version; operator recovery required".into(),
                )
            })?;
        let receipt = shared_types::UserAppOperationLeaseReceipt::Kubernetes {
            service_type: service_type.clone(),
            namespace: self.namespace.clone(),
            name: name.clone(),
            uid: uid.clone(),
            resource_version: version.clone(),
            token,
        };
        Ok(Box::new(OperationLease {
            receipt,
            api,
            name,
            uid,
            version,
            released: false,
        }))
    }
}

fn operation_name(app_id: &str, family: &ServiceType) -> ContainerRuntimeResult<String> {
    let code = match family {
        ServiceType::Userapp => "prod",
        ServiceType::UserappBuilder => "builder",
        _ => {
            return Err(ContainerRuntimeError::ConfigurationError(
                "application operation requires UserApp family".into(),
            ));
        }
    };
    Ok(format!("rcoder-operation-{code}-{app_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn operation_names_separate_prefix_shaped_app_ids_across_families() {
        assert_ne!(
            operation_name("builder-foo", &ServiceType::Userapp).expect("prod"),
            operation_name("foo", &ServiceType::UserappBuilder).expect("builder")
        );
        assert_eq!(
            operation_name("builder-foo", &ServiceType::Userapp).expect("prod"),
            "rcoder-operation-prod-builder-foo"
        );
    }
}
