//! Per-application operation mutex. No expiration or automatic stale-owner takeover.
//! A crashed owner's ConfigMap requires operator recovery after proving quiescence.
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
    pub(super) async fn acquire_application_operation(
        &self,
        app_id: &str,
        service_type: &ServiceType,
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
        let operation = uuid::Uuid::new_v4().to_string();
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
                annotations: Some(std::collections::BTreeMap::from([(
                    "rcoder.io/operation-id".into(),
                    operation,
                )])),
                ..Default::default()
            },
            ..Default::default()
        };
        let created = api
            .create(&PostParams::default(), &object)
            .await
            .map_err(|error| match &error {
                kube::Error::Api(status) if status.code == 409 => ContainerRuntimeError::Conflict(
                    format!("application operation {name} is in progress"),
                ),
                _ => ContainerRuntimeError::K8sError(format!(
                    "acquire application operation {name}: {error}"
                )),
            })?;
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
        Ok(Box::new(OperationLease {
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
