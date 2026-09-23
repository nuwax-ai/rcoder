//! Private restart templates. Restoring a controller always begins at zero replicas.
use super::kubernetes_runtime::KubernetesRuntime;
use container_runtime_api::{
    ContainerRuntimeError as Error, ContainerRuntimeResult as Result, UserAppDeploymentRuntime as _,
};
use k8s_openapi::{
    ByteString,
    api::{apps::v1::StatefulSet, core::v1::Secret},
};
use kube::{Api, api::PostParams};
use sha2::{Digest as _, Sha256};
use shared_types::{
    AppResourceIdentity, AppResourceKind, BuilderControlTarget, BuilderRestartTemplate,
    UserAppOperationScope,
};
use std::collections::BTreeMap;

const RESTORE: &str = "rcoder.io/restart-archive-uid";
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Archive {
    source: BuilderControlTarget,
    workload: StatefulSet,
    volumes: Vec<AppResourceIdentity>,
}
fn fail(error: impl std::fmt::Display) -> Error {
    Error::K8sError(format!("Builder restart archive: {error}"))
}

impl KubernetesRuntime {
    pub(super) async fn remove_builder_restart_archive(
        &self,
        template: &BuilderRestartTemplate,
    ) -> Result<()> {
        template
            .source
            .validate()
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
        // DELETE may only set deletionTimestamp (for example with finalizers).
        // Keep the durable GC marker pending until this exact UID is gone.
        // A same-name replacement is not ours and must never be deleted.
        match secrets
            .get_opt(&template.archive.name)
            .await
            .map_err(fail)?
        {
            None => Ok(()),
            Some(current) if current.metadata.uid.as_ref() != Some(&template.archive.uid) => Ok(()),
            Some(_) => Err(Error::Conflict(
                "Restart archive deletion is still pending".into(),
            )),
        }
    }
    pub(super) async fn archive_builder_template(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<BuilderRestartTemplate> {
        target.validate().map_err(Error::ConfigurationError)?;
        let source = target
            .workload
            .as_ref()
            .filter(|r| r.kind == AppResourceKind::StatefulSet)
            .ok_or_else(|| Error::Conflict("Original builder controller is missing".into()))?;
        let workloads: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let workload = workloads.get(&source.name).await.map_err(fail)?;
        if workload.metadata.uid.as_ref() != Some(&source.uid)
            || workload.metadata.resource_version != source.resource_version
            || workload.metadata.deletion_timestamp.is_some()
        {
            return Err(Error::Conflict(
                "Builder changed before template capture".into(),
            ));
        }
        let volumes = self.capture_builder_volume_witness(target).await?;
        let mut after = self
            .capture_builder_compute_with_binding(
                &target.context,
                target.resource_binding.as_ref(),
                false,
            )
            .await?;
        // The requested rollout image belongs to the operation checkpoint, not
        // to the live controller being captured for its private restart archive.
        after.restart_image = target.restart_image.clone();
        if &after != target {
            return Err(Error::Conflict(
                "Builder changed during template capture".into(),
            ));
        }
        // Bounded DNS name independent of request ID length; namespace, lifecycle
        // and physical source all participate in the deterministic identity.
        // Existing checkpoints retain their explicit archive names and UIDs.
        let key = serde_json::to_vec(&(&self.namespace, &target.context, source)).map_err(fail)?;
        let digest: String = Sha256::digest(key)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let name = format!("rcoder-builder-restart.{}.{}", &digest[..32], &digest[32..]);
        let archive = Archive {
            source: target.clone(),
            workload,
            volumes: volumes.clone(),
        };
        let payload = serde_json::to_vec(&archive).map_err(fail)?;
        if payload.len() > 900_000 {
            return Err(Error::ConfigurationError(
                "Restart template exceeds archive limit".into(),
            ));
        }
        let object = Secret {
            metadata: kube::core::ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(self.namespace.clone()),
                labels: Some(BTreeMap::from([(
                    "rcoder.io/resource-type".into(),
                    "builder-restart-template".into(),
                )])),
                ..Default::default()
            },
            immutable: Some(true),
            type_: Some("Opaque".into()),
            data: Some(BTreeMap::from([(
                "template".into(),
                ByteString(payload.clone()),
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
            return Err(Error::Conflict("Restart archive content changed".into()));
        }
        let uid = saved
            .metadata
            .uid
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| Error::Conflict("Restart archive UID missing".into()))?;
        Ok(BuilderRestartTemplate {
            source: target.clone(),
            archive: AppResourceIdentity {
                kind: AppResourceKind::Secret,
                name,
                uid,
                resource_version: saved.metadata.resource_version,
            },
            volumes,
        })
    }

    pub(super) async fn restore_builder_template(
        &self,
        template: &BuilderRestartTemplate,
    ) -> Result<BuilderControlTarget> {
        template
            .source
            .validate()
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
            return Err(Error::Conflict("Restart archive identity changed".into()));
        }
        let payload = saved
            .data
            .as_ref()
            .and_then(|data| data.get("template"))
            .ok_or_else(|| Error::Conflict("Restart archive payload missing".into()))?;
        let archive: Archive = serde_json::from_slice(&payload.0).map_err(fail)?;
        if archive.source != template.source || archive.volumes != template.volumes {
            return Err(Error::Conflict(
                "Restart archive does not match checkpoint".into(),
            ));
        }
        let context = &template.source.context;
        let original = template
            .source
            .workload
            .as_ref()
            .ok_or_else(|| Error::Conflict("Restart source missing".into()))?;
        if archive.workload.metadata.name.as_ref() != Some(&original.name)
            || archive.workload.metadata.uid.as_ref() != Some(&original.uid)
            || archive.workload.metadata.resource_version != original.resource_version
        {
            return Err(Error::Conflict(
                "Archived controller does not match restart source".into(),
            ));
        }
        let workloads: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        if let Some(existing) = workloads.get_opt(&original.name).await.map_err(fail)? {
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
            self.confirm_builder_compute_absent(context).await?;
            self.verify_recovered_volumes(context, UserAppOperationScope::Dev, &template.volumes)
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
            // Existing claims remain independent of the replacement controller.
            spec.persistent_volume_claim_retention_policy = Some(
                k8s_openapi::api::apps::v1::StatefulSetPersistentVolumeClaimRetentionPolicy {
                    when_deleted: Some("Retain".into()),
                    when_scaled: Some("Retain".into()),
                },
            );
            match workloads.create(&PostParams::default(), &desired).await {
                Ok(_) => {}
                Err(kube::Error::Api(error)) if error.code == 409 => {}
                Err(error) => return Err(fail(error)),
            }
        }
        let current = workloads.get(&original.name).await.map_err(fail)?;
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
            .capture_builder_compute_with_binding(context, None, false)
            .await?;
        if restored
            .workload
            .as_ref()
            .is_none_or(|r| r.uid == original.uid)
        {
            return Err(Error::Conflict(
                "Restart did not produce a replacement controller".into(),
            ));
        }
        let volumes = self.capture_builder_volume_witness(&restored).await?;
        if volumes != template.volumes {
            return Err(Error::Conflict("Restart workspace volumes changed".into()));
        }
        Ok(restored)
    }
}
