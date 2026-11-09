//! Private restart templates. Restoring a controller always begins at zero replicas.
use super::kubernetes_runtime::KubernetesRuntime;
use container_runtime_api::{
    ContainerRuntimeError as Error, ContainerRuntimeResult as Result, UserAppDeploymentRuntime as _,
};
use k8s_openapi::{
    ByteString,
    api::{
        apps::v1::StatefulSet,
        core::v1::{PodSpec, Secret},
    },
    apimachinery::pkg::api::resource::Quantity,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    candidate_pod_spec: Option<PodSpec>,
}
fn fail(error: impl std::fmt::Display) -> Error {
    Error::K8sError(format!("Builder restart archive: {error}"))
}
async fn read_before<T>(
    deadline: tokio::time::Instant,
    phase: &str,
    read: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout_at(deadline, read).await.map_err(|_| {
        Error::Timeout(format!(
            "Builder restart observation budget exhausted during {phase}"
        ))
    })?
}

impl KubernetesRuntime {
    async fn start_retained_without_archive(
        &self,
        target: &BuilderControlTarget,
        volumes: &[AppResourceIdentity],
        deadline: tokio::time::Instant,
    ) -> Result<Option<shared_types::ContainerBasicInfo>> {
        if !read_before(
            deadline,
            "retained stop confirmation",
            self.reconcile_builder_stop_receipt(target),
        )
        .await?
        {
            return Err(Error::Conflict(
                "Lost restart archive requires reconciliation of the original compute write".into(),
            ));
        }
        let mut fresh = read_before(
            deadline,
            "retained controller capture",
            self.capture_builder_compute_with_binding(
                &target.context,
                target.resource_binding.as_ref(),
                false,
            ),
        )
        .await?;
        if fresh.workload.as_ref().map(|value| &value.uid)
            != target.workload.as_ref().map(|value| &value.uid)
        {
            return Err(Error::Conflict(
                "Retained builder changed before archive recovery".into(),
            ));
        }
        if read_before(
            deadline,
            "retained data binding",
            self.capture_builder_volume_witness(&fresh),
        )
        .await?
            != volumes
        {
            return Err(Error::Conflict(
                "Retained builder data volumes changed during archive recovery".into(),
            ));
        }
        fresh.restart_image = target.restart_image.clone();
        fresh.restart_runtime_workspace = target.restart_runtime_workspace.clone();
        fresh.builder_startup_mode = target.builder_startup_mode;
        tracing::warn!(operation_id = %target.context.operation_id,
            "historical restart archive unavailable; continuing confirmed stopped owned compute");
        let outcome = self
            .apply_builder_compute_mode_inner(
                &fresh,
                true,
                true,
                true,
                None,
                Some(deadline.into_std()),
            )
            .await;
        self.report_control_outcome(&fresh, true, true, &outcome);
        outcome
    }
    fn existing_restart_template(
        &self,
        saved: &Secret,
        target: &BuilderControlTarget,
        volumes: &[AppResourceIdentity],
    ) -> Result<BuilderRestartTemplate> {
        let payload = saved
            .data
            .as_ref()
            .and_then(|data| data.get("template"))
            .ok_or_else(|| Error::Conflict("Restart archive payload missing".into()))?;
        let archive: Archive = serde_json::from_slice(&payload.0).map_err(fail)?;
        if saved.immutable != Some(true)
            || saved.metadata.deletion_timestamp.is_some()
            || archive.source.context != target.context
            || archive.source.workload.as_ref().map(|value| &value.uid)
                != target.workload.as_ref().map(|value| &value.uid)
            || archive.source.resource_binding != target.resource_binding
            || archive.volumes != volumes
        {
            return Err(Error::Conflict(
                "Restart archive belongs to another execution or data binding".into(),
            ));
        }
        Ok(BuilderRestartTemplate {
            source: archive.source,
            archive: AppResourceIdentity {
                kind: AppResourceKind::Secret,
                name: saved
                    .metadata
                    .name
                    .clone()
                    .ok_or_else(|| Error::Conflict("Restart archive name missing".into()))?,
                uid: saved
                    .metadata
                    .uid
                    .clone()
                    .filter(|uid| !uid.is_empty())
                    .ok_or_else(|| Error::Conflict("Restart archive UID missing".into()))?,
                resource_version: saved.metadata.resource_version.clone(),
            },
            volumes: volumes.to_vec(),
        })
    }
    fn builder_restart_candidate(
        &self,
        target: &BuilderControlTarget,
        original: &PodSpec,
    ) -> Result<PodSpec> {
        let params = container_runtime_api::ContainerCreateParams::builder()
            .project_id(target.context.app_id.clone())
            .builder_app_id(target.context.app_id.clone())
            .service_type(shared_types::ServiceType::UserappBuilder)
            .build();
        let apply_platform_recipe = target.restart_image.is_some();
        let mut candidate = if apply_platform_recipe {
            self.build_agent_pod_spec(
                &target.context.app_id,
                &shared_types::ServiceType::UserappBuilder,
                &params,
            )?
        } else {
            // Repair/recovery did not authorize a release rollout. Retain its
            // original image and configuration rather than reading new intent.
            original.clone()
        };
        if let Some(credential) = super::k8s_builder_control::retained_builder_credential(original)?
        {
            Self::install_builder_credential(&mut candidate, credential)?;
        }
        let original_agent = original
            .containers
            .iter()
            .find(|container| container.name == "agent")
            .ok_or_else(|| Error::Conflict("Restart original agent container missing".into()))?;
        let agent = candidate
            .containers
            .iter_mut()
            .find(|container| container.name == "agent")
            .ok_or_else(|| {
                Error::ConfigurationError("Restart candidate agent container missing".into())
            })?;
        agent.image = target
            .restart_image
            .clone()
            .or_else(|| original_agent.image.clone());
        agent.resources = original_agent.resources.clone();
        // A rollout that adopts the current platform template also adopts the
        // current platform memory-request policy: recompute exactly this one
        // field as min(1Gi, limit) from the retained limit, so a candidate
        // rebuilt from a pre-upgrade original (e.g. request=64Mi under a 6Gi
        // limit) no longer resurrects the outdated request. Everything else —
        // all limits and the cpu/ephemeral-storage requests — stays verbatim,
        // and shapes without a memory limit keep their original resources.
        if apply_platform_recipe
            && let Some(limit) = agent
                .resources
                .as_ref()
                .and_then(|resources| resources.limits.as_ref())
                .and_then(|limits| limits.get("memory"))
        {
            let request = super::k8s_app_helpers::min_memory_request(&limit.0);
            agent
                .resources
                .get_or_insert_default()
                .requests
                .get_or_insert_default()
                .insert("memory".to_string(), Quantity(request));
        }
        let env = agent.env.get_or_insert_default();
        // Existing application-owned entries absent from the platform recipe
        // remain verbatim, including valueFrom. Never silently remove them.
        for entry in original_agent.env.iter().flatten() {
            if !env.iter().any(|desired| desired.name == entry.name) {
                env.push(entry.clone());
            }
        }
        if let Some(root) = &target.restart_runtime_workspace {
            let env = agent.env.get_or_insert_default();
            env.retain(|entry| entry.name != "APP_CLI_RUNTIME_WORKSPACE");
            env.push(k8s_openapi::api::core::v1::EnvVar {
                name: "APP_CLI_RUNTIME_WORKSPACE".into(),
                value: Some(root.clone()),
                ..Default::default()
            });
        }
        // Per-request identity slots are not platform release configuration.
        let env = agent.env.get_or_insert_default();
        for name in ["TENANT_ID", "SPACE_ID", "ISOLATION_TYPE", "USER_ID"] {
            if let Some(entry) = original_agent
                .env
                .iter()
                .flatten()
                .find(|entry| entry.name == name)
            {
                env.retain(|entry| entry.name != name);
                env.push(entry.clone());
            }
        }
        if target.builder_startup_mode == shared_types::BuilderStartupMode::ManagementOnly {
            let env = agent.env.get_or_insert_default();
            env.retain(|entry| {
                entry.name != shared_types::APP_CLI_CONTROL_ONLY_ENV
                    && entry.name != shared_types::APP_CLI_MANAGEMENT_BOOT_ID_ENV
            });
            env.push(k8s_openapi::api::core::v1::EnvVar {
                name: shared_types::APP_CLI_CONTROL_ONLY_ENV.into(),
                value: Some("true".into()),
                ..Default::default()
            });
            env.push(k8s_openapi::api::core::v1::EnvVar {
                name: shared_types::APP_CLI_MANAGEMENT_BOOT_ID_ENV.into(),
                value: Some(target.context.operation_id.clone()),
                ..Default::default()
            });
        }
        super::k8s_builder_control::verify_bound_builder_storage(&candidate, original)?;
        // A release update cannot repoint any existing PVC. Additional storage
        // or mount migration needs its own explicit data contract.
        let claims = |spec: &PodSpec| -> BTreeMap<String, String> {
            spec.volumes
                .iter()
                .flatten()
                .filter_map(|volume| {
                    volume
                        .persistent_volume_claim
                        .as_ref()
                        .map(|claim| (volume.name.clone(), claim.claim_name.clone()))
                })
                .collect()
        };
        if claims(&candidate) != claims(original) {
            return Err(Error::Conflict(
                "Restart candidate changes persistent volume bindings".into(),
            ));
        }
        // Validate all names/defaults now, before the engine persists Stopping.
        super::k8s_builder_control::normalized_builder_pod_spec(&candidate)?;
        Ok(candidate)
    }

    pub(super) async fn start_builder_with_restart_template(
        &self,
        target: &BuilderControlTarget,
        template: &BuilderRestartTemplate,
    ) -> Result<Option<shared_types::ContainerBasicInfo>> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
        target.validate().map_err(Error::ConfigurationError)?;
        if target.context != template.source.context
            || template.archive.kind != AppResourceKind::Secret
        {
            return Err(Error::Conflict(
                "Builder restart context or archive kind differs".into(),
            ));
        }
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        let saved = read_before(deadline, "archive read", async {
            secrets.get_opt(&template.archive.name).await.map_err(fail)
        })
        .await?;
        let Some(saved) = saved else {
            return self
                .start_retained_without_archive(target, &template.volumes, deadline)
                .await;
        };
        if saved.metadata.uid.as_ref() != Some(&template.archive.uid)
            || saved.immutable != Some(true)
            || saved.metadata.deletion_timestamp.is_some()
        {
            return self
                .start_retained_without_archive(target, &template.volumes, deadline)
                .await;
        }
        let bytes = saved.data.as_ref().and_then(|data| data.get("template"));
        let Some(bytes) = bytes else {
            return self
                .start_retained_without_archive(target, &template.volumes, deadline)
                .await;
        };
        let archive: Archive = match serde_json::from_slice(&bytes.0) {
            Ok(archive) => archive,
            Err(error) => {
                tracing::warn!(%error, "historical builder restart payload cannot be decoded");
                return self
                    .start_retained_without_archive(target, &template.volumes, deadline)
                    .await;
            }
        };
        if archive.source != template.source || archive.volumes != template.volumes {
            return Err(Error::Conflict(
                "Restart archive does not match checkpoint".into(),
            ));
        }
        let Some(candidate) = archive.candidate_pod_spec else {
            // Old checkpoints contain no candidate intent. Preserve their old
            // image/workspace-only continuation without inventing new config.
            return self.start_builder_compute(target).await;
        };
        let source = template
            .source
            .workload
            .as_ref()
            .ok_or_else(|| Error::Conflict("Restart source controller missing".into()))?;
        if archive.workload.metadata.name.as_ref() != Some(&source.name)
            || archive.workload.metadata.uid.as_ref() != Some(&source.uid)
            || archive.workload.metadata.resource_version != source.resource_version
        {
            return Err(Error::Conflict(
                "Restart archived controller does not match source".into(),
            ));
        }
        let workload = target
            .workload
            .as_ref()
            .ok_or_else(|| Error::Conflict("Restart current controller missing".into()))?;
        let api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &self.namespace);
        let current = read_before(deadline, "controller read", async {
            api.get(&workload.name).await.map_err(fail)
        })
        .await?;
        if current.metadata.uid.as_ref() != Some(&workload.uid)
            || current.metadata.deletion_timestamp.is_some()
        {
            return Err(Error::Conflict(
                "Restart controller identity changed".into(),
            ));
        }
        if workload.uid != source.uid
            && current
                .metadata
                .annotations
                .as_ref()
                .and_then(|values| values.get(RESTORE))
                != Some(&template.archive.uid)
        {
            return Err(Error::Conflict(
                "Restart controller is not the captured or restored identity".into(),
            ));
        }
        let actual = current
            .spec
            .as_ref()
            .and_then(|spec| spec.template.spec.as_ref())
            .ok_or_else(|| Error::Conflict("Stopped builder template missing".into()))?;
        super::k8s_builder_control::verify_bound_builder_storage(&candidate, actual)?;
        if read_before(
            deadline,
            "volume identity read",
            self.capture_builder_volume_witness(target),
        )
        .await?
            != template.volumes
        {
            return Err(Error::Conflict(
                "Restart persistent volume identities changed".into(),
            ));
        }
        let receipt = serde_json::to_string(&target.context).map_err(fail)?;
        let previously_applied = current.spec.as_ref().and_then(|spec| spec.replicas) == Some(1)
            && current
                .metadata
                .annotations
                .as_ref()
                .and_then(|values| values.get("rcoder.io/compute-start-receipt"))
                == Some(&receipt)
            && super::k8s_builder_control::normalized_builder_pod_spec(actual)?
                == super::k8s_builder_control::normalized_builder_pod_spec(&candidate)?;
        if !previously_applied {
            if current.metadata.resource_version != workload.resource_version
                || current.spec.as_ref().and_then(|spec| spec.replicas) != Some(0)
            {
                return Err(Error::Conflict(
                    "Restart controller changed or is not stopped".into(),
                ));
            }
            let pod_name = self.agent_pod_name(
                &target.context.app_id,
                &shared_types::ServiceType::UserappBuilder,
            )?;
            if read_before(deadline, "old Pod exit read", async {
                self.pods().get_opt(&pod_name).await.map_err(fail)
            })
            .await?
            .is_some()
            {
                return Err(Error::Conflict(
                    "Original builder Pod has not exited before configuration update".into(),
                ));
            }
            let original_spec = archive
                .workload
                .spec
                .as_ref()
                .and_then(|spec| spec.template.spec.as_ref())
                .ok_or_else(|| Error::Conflict("Restart original template missing".into()))?;
            if super::k8s_builder_control::normalized_builder_pod_spec(actual)?
                != super::k8s_builder_control::normalized_builder_pod_spec(original_spec)?
            {
                return Err(Error::Conflict(
                    "Builder configuration changed after restart capture".into(),
                ));
            }
            let mut replacement = current.clone();
            replacement
                .metadata
                .annotations
                .get_or_insert_default()
                .insert("rcoder.io/compute-start-receipt".into(), receipt);
            let spec = replacement
                .spec
                .as_mut()
                .ok_or_else(|| Error::Conflict("Stopped builder spec missing".into()))?;
            spec.replicas = Some(1);
            spec.template.spec = Some(candidate.clone());
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Timeout(
                    "Builder restart budget exhausted before configuration dispatch".into(),
                ));
            }
            // Conditional PUT retains UID/RV and replaces the complete PodSpec.
            // An omitted optional field cannot survive as stale configuration.
            api.replace(&workload.name, &PostParams::default(), &replacement)
                .await
                .map_err(|error| {
                    super::builder_completion::k8s_error(
                        format!("Apply frozen builder restart configuration: {error}"),
                        error,
                    )
                })?;
        }
        let mut fresh = read_before(
            deadline,
            "started controller capture",
            self.capture_builder_compute_with_binding(
                &target.context,
                target.resource_binding.as_ref(),
                false,
            ),
        )
        .await?;
        if fresh.workload.as_ref().map(|value| &value.uid) != Some(&workload.uid) {
            return Err(Error::Conflict(
                "Builder replaced after configuration update".into(),
            ));
        }
        fresh.restart_image = target.restart_image.clone();
        fresh.restart_runtime_workspace = target.restart_runtime_workspace.clone();
        fresh.builder_startup_mode = target.builder_startup_mode;
        let outcome = self
            .apply_builder_compute_mode_inner(
                &fresh,
                true,
                true,
                true,
                None,
                Some(deadline.into_std()),
            )
            .await;
        self.report_control_outcome(&fresh, true, true, &outcome);
        let result = outcome?;
        let verified = read_before(deadline, "final configuration read", async {
            api.get(&workload.name).await.map_err(fail)
        })
        .await?;
        let verified_spec = verified
            .spec
            .as_ref()
            .and_then(|spec| spec.template.spec.as_ref())
            .ok_or_else(|| Error::Conflict("Started builder template missing".into()))?;
        if verified.metadata.uid.as_ref() != Some(&workload.uid)
            || super::k8s_builder_control::normalized_builder_pod_spec(verified_spec)?
                != super::k8s_builder_control::normalized_builder_pod_spec(&candidate)?
        {
            return Err(Error::Conflict(
                "Frozen builder configuration changed after start".into(),
            ));
        }
        Ok(result)
    }

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
        after.restart_runtime_workspace = target.restart_runtime_workspace.clone();
        after.builder_startup_mode = target.builder_startup_mode;
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
        let secrets: Api<Secret> = Api::namespaced(self.client.clone(), &self.namespace);
        if let Some(saved) = secrets.get_opt(&name).await.map_err(fail)? {
            // The first immutable archive is the operation's already fixed
            // intent. A retry must never compare it to today's rebuilt recipe.
            return self.existing_restart_template(&saved, target, &volumes);
        }
        let original_spec = workload
            .spec
            .as_ref()
            .and_then(|spec| spec.template.spec.as_ref())
            .ok_or_else(|| Error::Conflict("Builder source Pod template missing".into()))?;
        let candidate = self.builder_restart_candidate(target, original_spec)?;
        let archive = Archive {
            source: target.clone(),
            workload,
            volumes: volumes.clone(),
            candidate_pod_spec: Some(candidate),
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
        let saved = match secrets.create(&PostParams::default(), &object).await {
            Ok(saved) => saved,
            Err(kube::Error::Api(error)) if error.code == 409 => {
                secrets.get(&name).await.map_err(fail)?
            }
            Err(error) => return Err(fail(error)),
        };
        self.existing_restart_template(&saved, target, &volumes)
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
