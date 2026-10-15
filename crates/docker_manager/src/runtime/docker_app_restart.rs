//! Stopped Docker image replacement. The daemon holds the configuration (including
//! credentials); durable receipts contain identities only. A staging container is
//! never started before the coordinator commits its new UID.
use super::{
    docker_app_create::{recreate_body_from_inspect, validate_app_container_target},
    docker_runtime::{DockerRuntime, app_deployment_name},
};
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use sha2::{Digest as _, Sha256};
use shared_types::{UserAppComputeStartTarget, UserAppMutationTarget};

const SOURCE: &str = "rcoder.io/restart-source";
const INTENT: &str = "rcoder.io/restart-intent";

fn fail(error: impl std::fmt::Display) -> Error {
    Error::DockerError(format!("Prepare stopped application image: {error}"))
}
fn stopped(inspect: &bollard::models::ContainerInspectResponse) -> bool {
    inspect.state.as_ref().is_some_and(|state| {
        state.running == Some(false) && state.paused != Some(true) && state.restarting != Some(true)
    })
}
async fn inspect_optional(
    client: &bollard::Docker,
    id: &str,
) -> Result<Option<bollard::models::ContainerInspectResponse>> {
    match client.inspect_container(id, None).await {
        Ok(value) => Ok(Some(value)),
        Err(bollard::errors::Error::DockerResponseServerError {
            status_code: 404, ..
        }) => Ok(None),
        Err(error) => Err(fail(error)),
    }
}

impl DockerRuntime {
    pub(super) async fn prepare_docker_app_start(
        &self,
        target: &UserAppMutationTarget,
    ) -> Result<UserAppComputeStartTarget> {
        let inspect = self
            .inner
            .get_docker_client()
            .inspect_container(&target.resource.uid, None)
            .await
            .map_err(fail)?;
        verify_source(target, &inspect)?;
        Ok(UserAppComputeStartTarget {
            target: target.clone(),
            compute_start_single_write: false,
            volumes: super::docker_builder_restart::bind_witness(&inspect)?,
            restart_image: None,
        })
    }

    pub(super) async fn replace_stopped_app(
        &self,
        prior: &UserAppComputeStartTarget,
    ) -> Result<Option<UserAppMutationTarget>> {
        let Some(image) = prior.restart_image.as_deref() else {
            return Ok(None);
        };
        prior
            .target
            .context
            .validate_identity(&prior.target.context.app_id)
            .map_err(Error::ConfigurationError)?;
        if prior.target.resource.name != app_deployment_name(&prior.target.context.app_id) {
            return Err(Error::Conflict("Restart target name differs".into()));
        }
        let client = self.inner.get_docker_client();
        let payload = serde_json::to_vec(prior).map_err(fail)?;
        let intent: String = Sha256::digest(&payload)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let staging_name = format!("{}-restart-{}", prior.target.resource.name, &intent[..24]);
        let current = inspect_optional(client, &prior.target.resource.name).await?;
        let staged = match &current {
            Some(value) if value.id.as_deref() != Some(&prior.target.resource.uid) => {
                Some(value.clone())
            }
            _ => inspect_optional(client, &staging_name).await?,
        };
        let staged = if let Some(staged) = staged {
            verify_replacement(prior, &intent, &staged)?;
            staged
        } else {
            let source = inspect_optional(client, &prior.target.resource.uid)
                .await?
                .ok_or_else(|| {
                    Error::Conflict("Restart source and replacement are both absent".into())
                })?;
            verify_source(&prior.target, &source)?;
            if !stopped(&source)
                || !super::docker_compute_receipt::matches_app_stop(&prior.target).await?
            {
                return Err(Error::Conflict(
                    "Original application stop is not confirmed".into(),
                ));
            }
            let config = source
                .config
                .as_ref()
                .ok_or_else(|| fail("Source config missing"))?;
            if config.image.as_deref() == Some(image) {
                return Ok(None);
            }
            let witnessed = self.prepare_docker_app_start(&prior.target).await?;
            prior
                .verify_same_volumes(&witnessed)
                .map_err(Error::Conflict)?;
            self.inner.ensure_image_exists(image).await.map_err(fail)?;
            let mut body = recreate_body_from_inspect(config, source.host_config.clone(), image);
            let labels = body.labels.get_or_insert_default();
            labels.extend(prior.target.context.resource_metadata());
            labels.insert(SOURCE.into(), prior.target.resource.uid.clone());
            labels.insert(INTENT.into(), intent.clone());
            // Only one observer may send create. An uncertain create is observed
            // by its deterministic name, never retried after that name is renamed.
            if !super::docker_compute_receipt::claim_app_replacement(&prior.target).await? {
                return Err(Error::Conflict("Original replacement create requires observation; retry recovery after it is visible".into()));
            }
            let created = client
                .create_container(
                    Some(bollard::query_parameters::CreateContainerOptions {
                        name: Some(staging_name.clone()),
                        platform: self.inner.config.default_platform.clone(),
                    }),
                    body,
                )
                .await
                .map_err(fail)?;
            let staged = client
                .inspect_container(&created.id, None)
                .await
                .map_err(fail)?;
            verify_replacement(prior, &intent, &staged)?;
            staged
        };
        let replacement_uid = staged
            .id
            .as_deref()
            .ok_or_else(|| fail("Replacement ID missing"))?;
        if let Some(source) = inspect_optional(client, &prior.target.resource.uid).await? {
            verify_source(&prior.target, &source)?;
            if !stopped(&source) {
                return Err(Error::Conflict(
                    "Original application is running again".into(),
                ));
            }
            client
                .remove_container(
                    &prior.target.resource.uid,
                    Some(bollard::query_parameters::RemoveContainerOptions {
                        force: false,
                        v: false,
                        ..Default::default()
                    }),
                )
                .await
                .map_err(fail)?;
        }
        if inspect_optional(client, &prior.target.resource.uid)
            .await?
            .is_some()
        {
            return Err(Error::Conflict(
                "Original application removal is not confirmed".into(),
            ));
        }
        if staged
            .name
            .as_deref()
            .map(|name| name.trim_start_matches('/'))
            != Some(prior.target.resource.name.as_str())
        {
            client
                .rename_container(
                    replacement_uid,
                    bollard::query_parameters::RenameContainerOptions {
                        name: prior.target.resource.name.clone(),
                    },
                )
                .await
                .map_err(fail)?;
        }
        let current = client
            .inspect_container(&prior.target.resource.name, None)
            .await
            .map_err(fail)?;
        verify_replacement(prior, &intent, &current)?;
        if current.id.as_deref() != Some(replacement_uid) {
            return Err(Error::Conflict("Replacement identity changed".into()));
        }
        let mut target = prior.target.clone();
        target.resource.uid = replacement_uid.into();
        super::docker_compute_receipt::finish_app_replacement(&prior.target).await?;
        Ok(Some(target))
    }
}

fn verify_source(
    target: &UserAppMutationTarget,
    inspect: &bollard::models::ContainerInspectResponse,
) -> Result<()> {
    let uid = validate_app_container_target(&target.context.app_id, inspect)?;
    if uid != target.resource.uid {
        return Err(Error::Conflict("Restart source identity differs".into()));
    }
    // The coordinator already validated a possibly adopted lifecycle binding.
    // Keep that authority attached to this exact UID instead of rejecting old
    // immutable Docker lifecycle labels after a legitimate adoption.
    Ok(())
}

fn verify_replacement(
    prior: &UserAppComputeStartTarget,
    intent: &str,
    inspect: &bollard::models::ContainerInspectResponse,
) -> Result<()> {
    validate_app_container_target(&prior.target.context.app_id, inspect)?;
    let config = inspect
        .config
        .as_ref()
        .ok_or_else(|| fail("Replacement config missing"))?;
    let labels = config
        .labels
        .as_ref()
        .ok_or_else(|| fail("Replacement labels missing"))?;
    if labels.get(SOURCE) != Some(&prior.target.resource.uid)
        || labels.get(INTENT).map(String::as_str) != Some(intent)
        || config.image != prior.restart_image
        || !stopped(inspect)
        || super::docker_builder_restart::bind_witness(inspect)? != prior.volumes
    {
        return Err(Error::Conflict(
            "Stopped replacement does not match original restart intent".into(),
        ));
    }
    prior
        .target
        .context
        .validate_application_metadata(
            &labels.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        )
        .map_err(Error::Conflict)
}

#[cfg(all(test, feature = "deploy-host"))]
mod tests {
    use super::*;
    use container_runtime_api::{AgentContainerRuntime, UserAppDeploymentRuntime};
    use std::{collections::HashMap, sync::Arc};

    /// A bounded real-daemon contract, deliberately separate from ordinary unit runs.
    #[tokio::test]
    #[ignore = "requires local Docker, alpine:3.20/alpine:3.22.4 and RCODER_OPERATION_RECEIPT_ROOT"]
    async fn docker_image_roll_preserves_data_and_recovers_address() {
        assert!(std::env::var_os("RCODER_OPERATION_RECEIPT_ROOT").is_some());
        let runtime = DockerRuntime::new(Arc::new(
            crate::DockerManager::new(crate::DockerManagerConfig::default())
                .await
                .unwrap(),
        ));
        let client = runtime.inner.get_docker_client();
        let app_id = format!("review{}", uuid::Uuid::new_v4().simple());
        let context = shared_types::UserAppExecutionContext {
            app_id: app_id.clone(),
            lifecycle_id: uuid::Uuid::new_v4().to_string(),
            operation_id: uuid::Uuid::new_v4().to_string(),
            executor_id: "review".into(),
            request_fingerprint: "a".repeat(64),
        };
        let name = app_deployment_name(&app_id);
        let volume = tempfile::tempdir().unwrap();
        std::fs::write(volume.path().join("retained"), "original data").unwrap();
        let mut labels: HashMap<String, String> = context.resource_metadata().into_iter().collect();
        labels.insert(
            shared_types::USERAPP_DOCKER_APP_ID_LABEL.into(),
            app_id.clone(),
        );
        labels.insert(
            "service-type".into(),
            shared_types::ServiceType::Userapp.to_string(),
        );
        labels.insert("managed-by".into(), "rcoder-app-manager".into());
        let body = bollard::models::ContainerCreateBody {
            image: Some("alpine:3.20".into()),
            cmd: Some(vec!["sleep".into(), "600".into()]),
            env: Some(vec!["PGPASSWORD=fixture-preserved".into()]),
            labels: Some(labels),
            host_config: Some(bollard::models::HostConfig {
                binds: Some(vec![format!("{}:/workspace", volume.path().display())]),
                port_bindings: Some(HashMap::from([(
                    "8080/tcp".into(),
                    Some(vec![bollard::models::PortBinding {
                        host_ip: Some("127.0.0.1".into()),
                        host_port: Some("0".into()),
                    }]),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        };
        let created = client
            .create_container(
                Some(bollard::query_parameters::CreateContainerOptions {
                    name: Some(name.clone()),
                    platform: runtime.inner.config.default_platform.clone(),
                }),
                body,
            )
            .await
            .unwrap();
        let result: anyhow::Result<()> = async {
            let target = runtime.capture_app_mutation_target(&context, None).await?;
            let mut prior = runtime.prepare_app_compute_start(&target).await?;
            prior.restart_image = Some("alpine:3.22.4".into());
            runtime.start_app_target(&target).await?;
            assert!(runtime.captured_start_is_running(&target).await?);
            runtime.stop_app_target(&target, true).await?;
            assert!(runtime.reconcile_app_compute_stop(&target).await?);
            let fresh = runtime
                .replace_stopped_app_image(&prior)
                .await?
                .expect("replacement");
            assert_ne!(fresh.resource.uid, created.id);
            let replay = runtime
                .replace_stopped_app_image(&prior)
                .await?
                .expect("replay replacement");
            assert_eq!(fresh, replay);
            let inspect = client.inspect_container(&fresh.resource.uid, None).await?;
            assert!(
                stopped(&inspect),
                "must not start before the new UID is committed"
            );
            assert_eq!(
                inspect.config.as_ref().unwrap().image.as_deref(),
                Some("alpine:3.22.4")
            );
            assert!(
                inspect
                    .config
                    .as_ref()
                    .unwrap()
                    .env
                    .as_ref()
                    .unwrap()
                    .contains(&"PGPASSWORD=fixture-preserved".into())
            );
            assert_eq!(
                std::fs::read_to_string(volume.path().join("retained"))?,
                "original data"
            );
            let mut prepared = runtime.prepare_app_compute_start(&fresh).await?;
            prior
                .verify_same_volumes(&prepared)
                .map_err(anyhow::Error::msg)?;
            prepared.restart_image = prior.restart_image;
            runtime.start_app_compute(&prepared).await?;
            assert!(runtime.reconcile_app_compute_start(&fresh).await?);
            shared_types::published::unregister_if_physical(&name, &fresh.resource.uid);
            assert!(shared_types::published::resolve_published_addr(&name, 8080).is_err());
            let info = shared_types::ContainerBasicInfo {
                container_id: fresh.resource.uid.clone(),
                container_name: name.clone(),
                container_ip: String::new(),
                internal_port: 8080,
                external_port: 0,
                project_id: app_id.clone(),
                status: "Running".into(),
                created_at: chrono::Utc::now(),
                service_url: String::new(),
                workload_uid: None,
            };
            runtime.refresh_container_reach(&info).await?;
            assert!(shared_types::published::resolve_published_addr(&name, 8080).is_ok());
            assert!(
                inspect_optional(client, &target.resource.uid)
                    .await?
                    .is_none()
            );
            Ok(())
        }
        .await;
        // Clean only this test's unique resources, even if a contract assertion failed as an error.
        let listed = client
            .list_containers(Some(bollard::query_parameters::ListContainersOptions {
                all: true,
                filters: Some(HashMap::from([(
                    "label".into(),
                    vec![format!(
                        "{}={app_id}",
                        shared_types::USERAPP_DOCKER_APP_ID_LABEL
                    )],
                )])),
                ..Default::default()
            }))
            .await
            .unwrap();
        for container in listed {
            if let Some(id) = container.id {
                client
                    .remove_container(
                        &id,
                        Some(bollard::query_parameters::RemoveContainerOptions {
                            force: true,
                            v: false,
                            ..Default::default()
                        }),
                    )
                    .await
                    .unwrap();
            }
        }
        runtime
            .cleanup_compute_receipt_files(&context)
            .await
            .unwrap();
        result.unwrap();
    }
}
