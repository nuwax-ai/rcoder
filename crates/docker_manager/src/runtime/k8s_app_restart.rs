//! Private application (prod) restart templates. Restoring a Deployment
//! always begins at zero replicas, mirroring the builder contract: the
//! platform's checkpoint CAS must succeed before the replacement starts.
//! The archived Deployment spec references secrets indirectly, so the
//! archive never carries plaintext credentials.
use super::kubernetes_runtime::KubernetesRuntime;
use container_runtime_api::{
    ContainerRuntimeError as Error, ContainerRuntimeResult as Result, UserAppDeploymentRuntime as _,
};
use k8s_openapi::api::{apps::v1::Deployment, core::v1::Secret};
use kube::{Api, api::PostParams};
use sha2::{Digest as _, Sha256};
use shared_types::{
    AppResourceIdentity, AppResourceKind, AppRestartTemplate, UserAppOperationScope,
};
use std::collections::BTreeMap;

const RESTORE: &str = "rcoder.io/restart-archive-uid";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Archive {
    source: shared_types::UserAppMutationTarget,
    workload: Deployment,
    volumes: Vec<AppResourceIdentity>,
}

fn fail(error: impl std::fmt::Display) -> Error {
    Error::K8sError(format!("Application restart archive: {error}"))
}

impl KubernetesRuntime {
    pub(super) async fn remove_app_restart_archive(
        &self,
        template: &AppRestartTemplate,
    ) -> Result<()> {
        template
            .source
            .context
            .validate_identity(&template.source.context.app_id)
            .map_err(Error::ConfigurationError)?;
        if template.archive.kind != AppResourceKind::Secret || template.archive.uid.is_empty() {
            return Err(Error::Conflict("Invalid restart archive identity".into()));
        }
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        let Some(saved) = secrets
            .get_opt(&template.archive.name)
            .await
            .map_err(fail)?
        else {
            return Ok(());
        };
        if saved.metadata.uid.as_ref() != Some(&template.archive.uid) {
            // The archived UID is already gone, including when a previous
            // DELETE succeeded but its response was lost. Leave the new object.
            return Ok(());
        }
        let parameters = kube::api::DeleteParams {
            preconditions: Some(kube::api::Preconditions {
                uid: Some(template.archive.uid.clone()),
                resource_version: saved.metadata.resource_version,
            }),
            ..Default::default()
        };
        match secrets.delete(&template.archive.name, &parameters).await {
            Ok(_) => {}
            Err(kube::Error::Api(error)) if error.code == 404 => return Ok(()),
            Err(error) => return Err(fail(error)),
        }
        // DELETE may only set deletionTimestamp. Keep the cleanup pending
        // until this exact UID is gone; a replacement is never ours to delete.
        match secrets
            .get_opt(&template.archive.name)
            .await
            .map_err(fail)?
        {
            None => Ok(()),
            Some(current) if current.metadata.uid.as_ref() != Some(&template.archive.uid) => Ok(()),
            Some(_) => Err(Error::Conflict(
                "Application restart archive deletion is still pending".into(),
            )),
        }
    }

    pub(super) async fn archive_app_template(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> Result<Option<AppRestartTemplate>> {
        target
            .context
            .validate_identity(&target.context.app_id)
            .map_err(Error::ConfigurationError)?;
        let source = &target.resource;
        if source.kind != AppResourceKind::Deployment {
            return Err(Error::Conflict(
                "Original application controller is missing".into(),
            ));
        }
        let deployments = self.deployments_api();
        let workload = deployments.get(&source.name).await.map_err(fail)?;
        if workload.metadata.uid.as_ref() != Some(&source.uid)
            || workload.metadata.resource_version != source.resource_version
            || workload.metadata.deletion_timestamp.is_some()
        {
            return Err(Error::Conflict(
                "Application changed before template capture".into(),
            ));
        }
        // Existing preparation validates the full identity chain (Deployment
        // annotations plus PVC lifecycle) and captures the volume witnesses.
        let prepared = self.prepare_captured_compute_start(target).await?;
        // Drift check: the captured identity must still hold after the reads.
        let after = self
            .capture_stop_target(&target.context, None)
            .await
            .map_err(fail)?;
        if after.resource != *source {
            return Err(Error::Conflict(
                "Application changed during template capture".into(),
            ));
        }
        let volumes = prepared.volumes.clone();
        let key = serde_json::to_vec(&(&self.namespace, &target.context, source)).map_err(fail)?;
        let digest: String = Sha256::digest(key)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let name = format!("rcoder-app-restart.{}.{}", &digest[..32], &digest[32..]);
        let archive = Archive {
            source: target.clone(),
            workload,
            volumes: volumes.clone(),
        };
        let payload = serde_json::to_vec(&archive).map_err(fail)?;
        if payload.len() > 900_000 {
            return Err(Error::ConfigurationError(
                "Application restart template exceeds archive limit".into(),
            ));
        }
        let object = Secret {
            metadata: kube::core::ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(BTreeMap::from([(
                    "rcoder.io/resource-type".into(),
                    "app-restart-template".into(),
                )])),
                ..Default::default()
            },
            immutable: Some(true),
            type_: Some("Opaque".into()),
            data: Some(BTreeMap::from([(
                "template".into(),
                k8s_openapi::ByteString(payload.clone()),
            )])),
            ..Default::default()
        };
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        let saved = match secrets.create(&PostParams::default(), &object).await {
            Ok(saved) => saved,
            Err(kube::Error::Api(error)) if error.code == 409 => {
                secrets.get(&name).await.map_err(fail)?
            }
            Err(error) => return Err(fail(error)),
        };
        if saved.immutable != Some(true)
            || saved.metadata.deletion_timestamp.is_some()
            || saved
                .data
                .as_ref()
                .and_then(|data| data.get("template"))
                .map(|value| &value.0)
                != Some(&payload)
        {
            return Err(Error::Conflict(
                "Application restart archive content changed".into(),
            ));
        }
        let uid = saved
            .metadata
            .uid
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| Error::Conflict("Restart archive UID missing".into()))?;
        Ok(Some(AppRestartTemplate {
            source: target.clone(),
            archive: AppResourceIdentity {
                kind: AppResourceKind::Secret,
                name,
                uid,
                resource_version: saved.metadata.resource_version,
            },
            volumes,
        }))
    }

    pub(super) async fn restore_app_template(
        &self,
        template: &AppRestartTemplate,
    ) -> Result<shared_types::UserAppMutationTarget> {
        template
            .source
            .context
            .validate_identity(&template.source.context.app_id)
            .map_err(Error::ConfigurationError)?;
        if template.archive.kind != AppResourceKind::Secret {
            return Err(Error::Conflict("Restart archive kind differs".into()));
        }
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        let saved = secrets.get(&template.archive.name).await.map_err(fail)?;
        if saved.metadata.uid.as_ref() != Some(&template.archive.uid)
            || saved.immutable != Some(true)
            || saved.metadata.deletion_timestamp.is_some()
        {
            return Err(Error::Conflict(
                "Application restart archive identity changed".into(),
            ));
        }
        let payload = saved
            .data
            .as_ref()
            .and_then(|data| data.get("template"))
            .ok_or_else(|| Error::Conflict("Restart archive payload missing".into()))?;
        let archive: Archive = serde_json::from_slice(&payload.0).map_err(fail)?;
        if archive.source != template.source || archive.volumes != template.volumes {
            return Err(Error::Conflict(
                "Application restart archive does not match checkpoint".into(),
            ));
        }
        let context = &template.source.context;
        let original = &template.source.resource;
        if archive.workload.metadata.name.as_ref() != Some(&original.name)
            || archive.workload.metadata.uid.as_ref() != Some(&original.uid)
            || archive.workload.metadata.resource_version != original.resource_version
        {
            return Err(Error::Conflict(
                "Archived controller does not match restart source".into(),
            ));
        }
        let deployments = self.deployments_api();
        if let Some(existing) = deployments.get_opt(&original.name).await.map_err(fail)? {
            if existing
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(RESTORE))
                != Some(&template.archive.uid)
            {
                return Err(Error::Conflict(
                    "Another controller occupies restart target".into(),
                ));
            }
        } else {
            if !self.app_compute_absent(context).await? {
                return Err(Error::Conflict(
                    "Original application compute still exists".into(),
                ));
            }
            self.verify_recovered_volumes(context, UserAppOperationScope::Prod, &template.volumes)
                .await?;
            let mut desired = archive.workload;
            let labels = desired.metadata.labels.take();
            let mut annotations = desired.metadata.annotations.take().unwrap_or_default();
            annotations.remove("rcoder.io/compute-stop-receipt");
            annotations.remove("rcoder.io/compute-start-receipt");
            annotations.insert(RESTORE.into(), template.archive.uid.clone());
            annotations.extend(context.resource_metadata());
            desired.metadata = kube::core::ObjectMeta {
                name: Some(original.name.clone()),
                namespace: Some(self.namespace.clone()),
                labels,
                annotations: Some(annotations),
                ..Default::default()
            };
            desired.status = None;
            let spec = desired
                .spec
                .as_mut()
                .ok_or_else(|| Error::Conflict("Restart template spec missing".into()))?;
            spec.replicas = Some(0);
            spec.template
                .metadata
                .get_or_insert_default()
                .annotations
                .get_or_insert_default()
                .extend(context.resource_metadata());
            match deployments.create(&PostParams::default(), &desired).await {
                Ok(_) => {}
                Err(kube::Error::Api(error)) if error.code == 409 => {}
                Err(error) => return Err(fail(error)),
            }
        }
        let current = deployments.get(&original.name).await.map_err(fail)?;
        if current
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(RESTORE))
            != Some(&template.archive.uid)
        {
            return Err(Error::Conflict(
                "Replacement controller archive identity differs".into(),
            ));
        }
        let restored = self
            .capture_stop_target(context, None)
            .await
            .map_err(fail)?;
        if restored.resource.uid == original.uid {
            return Err(Error::Conflict(
                "Application restart did not produce a replacement controller".into(),
            ));
        }
        let prepared = self.prepare_captured_compute_start(&restored).await?;
        if prepared.volumes != template.volumes {
            return Err(Error::Conflict(
                "Application workspace volumes changed".into(),
            ));
        }
        Ok(restored)
    }
}
