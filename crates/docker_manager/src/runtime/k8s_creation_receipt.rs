//! Immutable acknowledgement of one completed builder creation, independent
//! of the controller lifetime and the subsequently released operation lease.
use super::k8s_service::K8sServiceOps as _;
use super::{
    builder_creation_receipt::BuilderCreationReceipt, kubernetes_runtime::KubernetesRuntime,
};
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use k8s_openapi::api::core::v1::{ConfigMap, Service};
use kube::{Api, api::PostParams};
use shared_types::{UserAppExecutionContext, UserAppOperationLeaseReceipt};
use std::collections::BTreeMap;

fn name(context: &UserAppExecutionContext) -> Result<String> {
    context
        .validate_identity(&context.app_id)
        .map_err(Error::ConfigurationError)?;
    // Reversible hex, split into valid DNS labels. Operation IDs are globally
    // unique in the ledger; payload validation also checks app and executor.
    let parts: Vec<String> = context
        .operation_id
        .as_bytes()
        .chunks(20)
        .map(|chunk| chunk.iter().map(|byte| format!("{byte:02x}")).collect())
        .collect();
    Ok(format!("rcoder-builder-result.{}", parts.join(".")))
}
const SERVICE_COMPLETION: &str = "rcoder.io/builder-creation-completion";
impl KubernetesRuntime {
    pub(super) async fn commit_builder_service_creation(
        &self,
        receipt: &BuilderCreationReceipt,
    ) -> Result<()> {
        receipt.validate()?;
        if !matches!(&receipt.lease, UserAppOperationLeaseReceipt::Kubernetes { namespace, .. } if namespace == &self.namespace)
        {
            return Err(Error::Conflict(
                "Builder Service receipt namespace differs".into(),
            ));
        }
        let context = &receipt.target.context;
        let name =
            self.agent_service_name(&context.app_id, &shared_types::ServiceType::UserappBuilder)?;
        let payload = serde_json::to_string(receipt)
            .map_err(|error| Error::ConfigurationError(error.to_string()))?;
        if payload.len() > 65536 {
            return Err(Error::ConfigurationError(
                "Service creation receipt exceeds limit".into(),
            ));
        }
        let mut desired = super::k8s_service::agent_service_object(
            &self.namespace,
            &name,
            &context.app_id,
            &shared_types::ServiceType::UserappBuilder,
        );
        desired
            .metadata
            .annotations
            .get_or_insert_default()
            .insert(SERVICE_COMPLETION.into(), payload.clone());
        let api: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        match api
            .get_opt(&name)
            .await
            .map_err(|error| Error::K8sError(format!("Inspect creation Service: {error}")))?
        {
            Some(existing) => {
                super::k8s_service::validate_builder_service(&existing, &context.app_id, false)?;
                let uid = existing
                    .metadata
                    .uid
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| Error::Conflict("Creation Service UID missing".into()))?;
                let version = existing
                    .metadata
                    .resource_version
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| Error::Conflict("Creation Service version missing".into()))?;
                let ports = desired.spec.and_then(|spec| spec.ports).ok_or_else(|| {
                    Error::ConfigurationError("Creation Service ports missing".into())
                })?;
                let body = serde_json::json!({"metadata": {"uid": uid, "resourceVersion": version,
                    "annotations": {(SERVICE_COMPLETION): payload}}, "spec": {"ports": ports}});
                api.patch(
                    &name,
                    &kube::api::PatchParams::default(),
                    &kube::api::Patch::Merge(body),
                )
                .await
                .map_err(|error| {
                    Error::K8sError(format!("Commit builder Service completion: {error}"))
                })?;
            }
            None => {
                api.create(&PostParams::default(), &desired)
                    .await
                    .map_err(|error| {
                        Error::K8sError(format!("Create builder Service completion: {error}"))
                    })?;
            }
        }
        Ok(())
    }

    pub(super) async fn read_builder_service_creation(
        &self,
        context: &UserAppExecutionContext,
    ) -> Result<Option<BuilderCreationReceipt>> {
        context
            .validate_identity(&context.app_id)
            .map_err(Error::ConfigurationError)?;
        let name =
            self.agent_service_name(&context.app_id, &shared_types::ServiceType::UserappBuilder)?;
        let api: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        let Some(service) = api.get_opt(&name).await.map_err(|error| {
            Error::K8sError(format!("Read builder Service completion: {error}"))
        })?
        else {
            return Ok(None);
        };
        super::k8s_service::validate_builder_service(&service, &context.app_id, false)?;
        let Some(payload) = service
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get(SERVICE_COMPLETION))
        else {
            return Ok(None);
        };
        if payload.len() > 65536 {
            return Err(Error::Conflict(
                "Service creation receipt exceeds limit".into(),
            ));
        }
        let receipt: BuilderCreationReceipt = serde_json::from_str(payload).map_err(|error| {
            Error::Conflict(format!("Decode Service creation receipt: {error}"))
        })?;
        receipt.validate()?;
        if receipt.target.context.operation_id != context.operation_id {
            return Ok(None);
        }
        if receipt.target.context != *context
            || !matches!(&receipt.lease,
            UserAppOperationLeaseReceipt::Kubernetes { namespace, .. } if namespace == &self.namespace)
        {
            return Err(Error::Conflict(
                "Service completion execution identity differs".into(),
            ));
        }
        Ok(Some(receipt))
    }

    pub(super) async fn save_builder_creation_receipt(
        &self,
        receipt: &BuilderCreationReceipt,
    ) -> Result<()> {
        receipt.validate()?;
        let UserAppOperationLeaseReceipt::Kubernetes { namespace, .. } = &receipt.lease else {
            return Err(Error::ConfigurationError(
                "Kubernetes creation lease required".into(),
            ));
        };
        if namespace != &self.namespace {
            return Err(Error::Conflict("Creation receipt namespace differs".into()));
        }
        let payload = serde_json::to_string(receipt).map_err(|error| {
            Error::ConfigurationError(format!("Encode creation receipt: {error}"))
        })?;
        if payload.len() > 65536 {
            return Err(Error::ConfigurationError(
                "Creation receipt exceeds limit".into(),
            ));
        }
        let object = ConfigMap {
            metadata: kube::core::ObjectMeta {
                name: Some(name(&receipt.target.context)?),
                namespace: Some(self.namespace.clone()),
                labels: Some(BTreeMap::from([(
                    "rcoder.io/resource-type".into(),
                    "builder-creation-receipt".into(),
                )])),
                ..Default::default()
            },
            immutable: Some(true),
            data: Some(BTreeMap::from([("receipt".into(), payload.clone())])),
            ..Default::default()
        };
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        match api.create(&PostParams::default(), &object).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(error)) if error.code == 409 => {
                let existing = self
                    .read_builder_creation_receipt(&receipt.target.context)
                    .await?
                    .ok_or_else(|| {
                        Error::Conflict("Creation receipt disappeared after conflict".into())
                    })?;
                let actual = serde_json::to_string(&existing)
                    .map_err(|error| Error::ConfigurationError(error.to_string()))?;
                if actual != payload {
                    return Err(Error::Conflict("Creation receipt changed identity".into()));
                }
                Ok(())
            }
            Err(error) => Err(Error::K8sError(format!(
                "Persist builder creation receipt: {error}"
            ))),
        }
    }
    pub(super) async fn read_builder_creation_receipt(
        &self,
        context: &UserAppExecutionContext,
    ) -> Result<Option<BuilderCreationReceipt>> {
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let Some(object) = api
            .get_opt(&name(context)?)
            .await
            .map_err(|error| Error::K8sError(format!("Read builder creation receipt: {error}")))?
        else {
            return Ok(None);
        };
        if object.immutable != Some(true)
            || object.metadata.deletion_timestamp.is_some()
            || object
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("rcoder.io/resource-type"))
                .map(String::as_str)
                != Some("builder-creation-receipt")
        {
            return Err(Error::Conflict(
                "Invalid builder creation receipt object".into(),
            ));
        }
        let payload = object
            .data
            .as_ref()
            .and_then(|data| data.get("receipt"))
            .ok_or_else(|| Error::Conflict("Builder creation receipt payload missing".into()))?;
        if payload.len() > 65536 {
            return Err(Error::Conflict("Creation receipt exceeds limit".into()));
        }
        let receipt: BuilderCreationReceipt = serde_json::from_str(payload)
            .map_err(|error| Error::Conflict(format!("Decode creation receipt: {error}")))?;
        receipt.validate()?;
        if receipt.target.context != *context
            || !matches!(&receipt.lease,
            UserAppOperationLeaseReceipt::Kubernetes { namespace, .. } if namespace == &self.namespace)
        {
            return Err(Error::Conflict(
                "Creation receipt belongs to another execution".into(),
            ));
        }
        Ok(Some(receipt))
    }
}

impl KubernetesRuntime {
    pub(super) async fn save_builder_cancellation(
        &self,
        receipt: &super::builder_creation_receipt::BuilderCancellationReceipt,
    ) -> Result<()> {
        receipt.validate()?;
        if !matches!(&receipt.lease, UserAppOperationLeaseReceipt::Kubernetes { namespace, .. } if namespace == &self.namespace)
        {
            return Err(Error::Conflict(
                "Cancellation receipt namespace differs".into(),
            ));
        }
        let payload = serde_json::to_string(receipt)
            .map_err(|error| Error::ConfigurationError(error.to_string()))?;
        if payload.len() > 65536 {
            return Err(Error::ConfigurationError(
                "Cancellation receipt exceeds limit".into(),
            ));
        }
        let object = ConfigMap {
            metadata: kube::core::ObjectMeta {
                name: Some(format!("cancel.{}", name(&receipt.context)?)),
                namespace: Some(self.namespace.clone()),
                labels: Some(BTreeMap::from([(
                    "rcoder.io/resource-type".into(),
                    "builder-cancellation-receipt".into(),
                )])),
                ..Default::default()
            },
            immutable: Some(true),
            data: Some(BTreeMap::from([("receipt".into(), payload.clone())])),
            ..Default::default()
        };
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        match api.create(&PostParams::default(), &object).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(error)) if error.code == 409 => {
                let existing = self
                    .read_builder_cancellation(&receipt.context)
                    .await?
                    .ok_or_else(|| Error::Conflict("Cancellation receipt disappeared".into()))?;
                let actual = serde_json::to_string(&existing)
                    .map_err(|error| Error::ConfigurationError(error.to_string()))?;
                if actual != payload {
                    return Err(Error::Conflict(
                        "Cancellation receipt changed identity".into(),
                    ));
                }
                Ok(())
            }
            Err(error) => Err(Error::K8sError(format!(
                "Persist builder cancellation: {error}"
            ))),
        }
    }

    pub(super) async fn read_builder_cancellation(
        &self,
        context: &UserAppExecutionContext,
    ) -> Result<Option<super::builder_creation_receipt::BuilderCancellationReceipt>> {
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.namespace);
        let Some(object) = api
            .get_opt(&format!("cancel.{}", name(context)?))
            .await
            .map_err(|error| Error::K8sError(format!("Read builder cancellation: {error}")))?
        else {
            return Ok(None);
        };
        if object.immutable != Some(true)
            || object.metadata.deletion_timestamp.is_some()
            || object
                .metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("rcoder.io/resource-type"))
                .map(String::as_str)
                != Some("builder-cancellation-receipt")
        {
            return Err(Error::Conflict(
                "Invalid builder cancellation receipt object".into(),
            ));
        }
        let payload = object
            .data
            .as_ref()
            .and_then(|data| data.get("receipt"))
            .ok_or_else(|| Error::Conflict("Cancellation receipt payload missing".into()))?;
        if payload.len() > 65536 {
            return Err(Error::Conflict("Cancellation receipt exceeds limit".into()));
        }
        let receipt: super::builder_creation_receipt::BuilderCancellationReceipt =
            serde_json::from_str(payload).map_err(|error| {
                Error::Conflict(format!("Decode cancellation receipt: {error}"))
            })?;
        receipt.validate()?;
        if receipt.context != *context
            || !matches!(&receipt.lease, UserAppOperationLeaseReceipt::Kubernetes { namespace, .. } if namespace == &self.namespace)
        {
            return Err(Error::Conflict(
                "Cancellation receipt execution identity differs".into(),
            ));
        }
        Ok(Some(receipt))
    }
}
