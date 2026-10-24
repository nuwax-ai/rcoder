//! Docker inventory and exact-CID retirement. Automatic callers must prove
//! deployment ownership, persistent workspace coverage and current idleness.
use super::DockerRuntime;
use container_runtime_api::{
    AgentResourceIdentity, AgentResourceKind, AgentStopOutcome, AgentStopSnapshot,
    ComputeDataProtection, ContainerRuntimeError as Error, ContainerRuntimeResult as Result,
};
use shared_types::ServiceType;
use std::collections::HashMap;
use tracing::debug;

#[cfg(test)]
mod tests;

impl DockerRuntime {
    pub(super) fn agent_retirement_namespace(&self) -> String {
        let address = self
            .inner
            .config
            .docker_host
            .clone()
            .or_else(|| std::env::var("DOCKER_SOCKET_PATH").ok())
            .or_else(|| std::env::var("DOCKER_HOST").ok())
            .unwrap_or_else(|| "local".into());
        format!("docker:{address}")
    }
    pub(super) fn agent_retirement_name(
        &self,
        identifier: &str,
        family: &ServiceType,
    ) -> Result<String> {
        let prefix = self
            .inner
            .config
            .multi_image_config
            .get_service_config(family)
            .map(|config| config.container_prefix())
            .unwrap_or_else(|| family.container_prefix());
        crate::utils::DockerUtils::generate_container_name(prefix, identifier)
            .map_err(Error::ConfigurationError)
    }

    pub(super) async fn docker_agent_inventory(&self) -> Result<Vec<AgentStopSnapshot>> {
        let containers = self
            .inner
            .get_docker_client()
            .list_containers(Some(bollard::query_parameters::ListContainersOptions {
                all: true,
                filters: Some(HashMap::from([(
                    "label".into(),
                    vec!["identifier".into(), "service-type".into()],
                )])),
                ..Default::default()
            }))
            .await
            .map_err(|error| {
                Error::DockerError(format!("List managed Agent containers: {error}"))
            })?;
        use futures_util::{StreamExt, TryStreamExt, stream};
        stream::iter(containers)
            .map(|container| self.inspect_agent_candidate(container))
            .buffer_unordered(8)
            .try_filter_map(|snapshot| async move { Ok(snapshot) })
            .try_collect()
            .await
    }

    async fn inspect_agent_candidate(
        &self,
        container: bollard::models::ContainerSummary,
    ) -> Result<Option<AgentStopSnapshot>> {
        let Some(labels) = container.labels.as_ref() else {
            return Ok(None);
        };
        let Some(family) = labels
            .get("service-type")
            .and_then(|value| value.parse::<ServiceType>().ok())
        else {
            return Ok(None);
        };
        match family {
            ServiceType::Userapp => return Ok(None),
            ServiceType::WebAgentRunner
            | ServiceType::ComputerAgentRunner
            | ServiceType::ComputerNormalProject
            | ServiceType::UserappBuilder => {}
        }
        let Some(identifier) = labels.get("identifier").filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        let uid = container
            .id
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::DockerError("Managed Agent has no full container ID".into()))?;
        let name = container
            .names
            .as_ref()
            .and_then(|names| names.first())
            .map(|value| value.trim_start_matches('/').to_owned())
            .ok_or_else(|| Error::DockerError("Managed Agent has no container name".into()))?;
        // Live evidence needs one inspect per candidate: StartedAt resets
        // observation on same-CID restarts, and mounts decide whether managed
        // data survives container removal. An unreadable candidate is absent
        // this round, so observation continuity never accrues on stale data.
        let inspected = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            self.inner.get_docker_client().inspect_container(&uid, None),
        )
        .await
        {
            Ok(Ok(inspected)) => inspected,
            Ok(Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            })) => return Ok(None),
            outcome => {
                debug!(uid = %uid, ?outcome, "Managed Agent inspect is unavailable; skipping this round");
                return Ok(None);
            }
        };
        let started_at = inspected
            .state
            .as_ref()
            .and_then(|state| state.started_at.as_deref())
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let persistent_mounts = self
            .persistent_agent_workspace(&inspected, family, identifier)
            .await;
        let auto_remove = inspected
            .host_config
            .as_ref()
            .and_then(|config| config.auto_remove)
            .unwrap_or(false);
        // Inspect is the single identity/network truth, not the older
        // list response or an arbitrary HashMap network belonging to peers.
        let observed_labels = inspected.config.as_ref().and_then(|c| c.labels.as_ref());
        if inspected.id.as_deref() != Some(uid.as_str())
            || observed_labels.and_then(|l| l.get("identifier")) != Some(identifier)
            || observed_labels
                .and_then(|l| l.get("service-type"))
                .and_then(|s| s.parse::<ServiceType>().ok())
                != Some(family)
        {
            return Err(Error::Conflict(
                "Docker Agent identity changed during inventory".into(),
            ));
        }
        let network = self.inner.get_main_network_name().await;
        let endpoint = inspected
            .network_settings
            .as_ref()
            .and_then(|s| s.networks.as_ref())
            .and_then(|n| n.get(&network))
            .and_then(|n| n.ip_address.as_deref())
            .filter(|ip| !ip.is_empty())
            .map(|ip| format!("http://{ip}:{}", shared_types::GRPC_DEFAULT_PORT));
        let identity = AgentResourceIdentity {
            kind: AgentResourceKind::Container,
            name,
            uid,
            resource_version: started_at,
            owner_uid: None,
            created_at: container
                .created
                .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0)),
        };
        Ok(Some(AgentStopSnapshot {
            app_id_hint: None,
            namespace: Some(self.agent_retirement_namespace()),
            identifier: identifier.clone(),
            service_type: family.family_representative(),
            controller: None,
            pods: Vec::new(),
            container: Some(identity),
            services: Vec::new(),
            replicas: None,
            endpoint,
            compute_data_protection: Some(ComputeDataProtection {
                persistent_mounts,
                auto_remove,
            }),
        }))
    }

    pub(super) async fn capture_docker_agent_retirement(
        &self,
        identifier: &str,
        family: &ServiceType,
    ) -> Result<AgentStopSnapshot> {
        let mut matches = self
            .docker_agent_inventory()
            .await?
            .into_iter()
            .filter(|snapshot| {
                snapshot.identifier == identifier
                    && snapshot.service_type.same_container_family(*family)
            });
        let captured = matches.next();
        if matches.next().is_some() {
            return Err(Error::Conflict(
                "Multiple Docker Agent containers share this scope".into(),
            ));
        }
        if captured.is_none() {
            let name = self.agent_retirement_name(identifier, family)?;
            match self
                .inner
                .get_docker_client()
                .inspect_container(&name, None)
                .await
            {
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => {}
                Ok(_) => {
                    return Err(Error::Conflict(
                        "Unclassified Docker Agent exists at its canonical name".into(),
                    ));
                }
                Err(error) => {
                    return Err(Error::DockerError(format!(
                        "Observe unclassified Agent: {error}"
                    )));
                }
            }
        }
        Ok(captured.unwrap_or_else(|| AgentStopSnapshot {
            app_id_hint: None,
            namespace: Some(self.agent_retirement_namespace()),
            identifier: identifier.into(),
            service_type: family.family_representative(),
            controller: None,
            pods: Vec::new(),
            container: None,
            services: Vec::new(),
            replicas: None,
            endpoint: None,
            compute_data_protection: None,
        }))
    }

    pub(super) async fn retire_docker_agent_snapshot(
        &self,
        snapshot: &AgentStopSnapshot,
        automatic: bool,
    ) -> Result<AgentStopOutcome> {
        if snapshot.namespace.as_deref() != Some(self.agent_retirement_namespace().as_str())
            || snapshot.controller.is_some()
            || !snapshot.pods.is_empty()
            || !snapshot.services.is_empty()
            || snapshot.identifier.is_empty()
        {
            return Err(Error::Conflict(
                "Docker Agent retirement scope differs".into(),
            ));
        }
        match snapshot.service_type {
            ServiceType::Userapp => {
                return Err(Error::Conflict(
                    "Production UserApp is not an Agent retirement target".into(),
                ));
            }
            ServiceType::WebAgentRunner
            | ServiceType::ComputerAgentRunner
            | ServiceType::ComputerNormalProject
            | ServiceType::UserappBuilder => {}
        }
        let Some(target) = &snapshot.container else {
            return Ok(AgentStopOutcome::Complete);
        };
        if target.kind != AgentResourceKind::Container || target.uid.is_empty() {
            return Err(Error::Conflict(
                "Docker Agent physical identity is missing".into(),
            ));
        }
        let client = self.inner.get_docker_client();
        let observed = match client.inspect_container(&target.uid, None).await {
            Ok(observed) => observed,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => return Ok(AgentStopOutcome::Complete),
            Err(error) => {
                return Err(Error::DockerError(format!(
                    "Inspect captured Agent: {error}"
                )));
            }
        };
        let labels = observed
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref());
        if observed.id.as_deref() != Some(target.uid.as_str())
            || labels
                .and_then(|labels| labels.get("identifier"))
                .map(String::as_str)
                != Some(snapshot.identifier.as_str())
            || labels
                .and_then(|labels| labels.get("service-type"))
                .and_then(|value| value.parse::<ServiceType>().ok())
                .is_none_or(|family| !family.same_container_family(snapshot.service_type))
        {
            return Err(Error::Conflict(
                "Captured Docker Agent ownership changed".into(),
            ));
        }
        if automatic {
            validate_automatic_evidence(snapshot, &observed)?;
            if !self
                .persistent_agent_workspace(&observed, snapshot.service_type, &snapshot.identifier)
                .await
            {
                return Err(Error::Conflict(
                    "Docker workspace no longer has deployment-owned persistent coverage".into(),
                ));
            }
        }
        // Never remove volumes or resolve this captured CID again by name.
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            client.remove_container(
                &target.uid,
                Some(bollard::query_parameters::RemoveContainerOptions {
                    force: true,
                    v: false,
                    link: false,
                }),
            ),
        )
        .await
        .map_err(|_| Error::Timeout("Captured Agent removal outcome is unknown".into()))?
        .or_else(|error| match error {
            bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            } => Ok(()),
            error => Err(error),
        })
        .map_err(|error| Error::DockerError(format!("Remove captured Agent: {error}")))?;
        match client.inspect_container(&target.uid, None).await {
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                self.inner
                    .retire_container_cache(&target.uid)
                    .await
                    .map_err(|error| Error::DockerError(error.to_string()))?;
                Ok(AgentStopOutcome::Complete)
            }
            Ok(_) => Err(Error::Conflict(
                "Captured Agent remains present after removal".into(),
            )),
            Err(error) => Err(Error::DockerError(format!(
                "Confirm captured Agent absence: {error}"
            ))),
        }
    }
}

/// The creation-side workspace mount is also our deployment boundary. A socket,
/// config mount or another deployment's directory never authorizes removal.
impl DockerRuntime {
    async fn persistent_agent_workspace(
        &self,
        inspected: &bollard::models::ContainerInspectResponse,
        family: ServiceType,
        identifier: &str,
    ) -> bool {
        let Some(config) = self
            .inner
            .config
            .multi_image_config
            .get_service_config(&family)
        else {
            return false;
        };
        let resolution = config.effective_workspace_resolution_path();
        let Ok(host_root) =
            crate::path::resolve_container_path_to_host(std::path::Path::new(&resolution)).await
        else {
            return false;
        };
        persistent_agent_workspace_in_scope(inspected, family, identifier, config, &host_root)
    }
}

fn persistent_agent_workspace_in_scope(
    inspected: &bollard::models::ContainerInspectResponse,
    family: ServiceType,
    identifier: &str,
    config: &shared_types::ServiceImageConfig,
    host_root: &std::path::Path,
) -> bool {
    let Some(labels) = inspected.config.as_ref().and_then(|c| c.labels.as_ref()) else {
        return false;
    };
    if labels.contains_key("managed-by")
        || labels.contains_key(shared_types::USERAPP_DOCKER_APP_ID_LABEL)
        || shared_types::USERAPP_RESOURCE_IDENTITY_KEYS
            .iter()
            .any(|key| labels.contains_key(*key))
    {
        return false;
    }
    let Some(env) = inspected.config.as_ref().and_then(|c| c.env.as_ref()) else {
        return false;
    };
    let value = |key: &str| env.iter().find_map(|s| s.strip_prefix(&format!("{key}=")));
    let component = |key: &str| {
        value(key).filter(|v| {
            !v.is_empty()
                && std::path::Path::new(v).components().count() == 1
                && matches!(
                    std::path::Path::new(v).components().next(),
                    Some(std::path::Component::Normal(_))
                )
        })
    };
    let target_root = config.workspace_container_path();
    if value("PROJECT_WORKSPACE_BASE").is_some_and(|root| root != target_root) {
        return false;
    }
    let relative = match family {
        ServiceType::WebAgentRunner => {
            let Some(project) = component("PROJECT_ID") else {
                return false;
            };
            if project == identifier {
                std::path::PathBuf::from(project)
            } else {
                // Shared Web workloads use the isolation layout from the
                // original request, never identifier-as-project guessing.
                let Some(tenant) = component("TENANT_ID") else {
                    return false;
                };
                match value("ISOLATION_TYPE") {
                    Some("tenant") => std::path::PathBuf::from(tenant),
                    Some("space") => {
                        let Some(space) = component("SPACE_ID") else {
                            return false;
                        };
                        std::path::Path::new(tenant).join(space)
                    }
                    Some("project") => {
                        let Some(space) = component("SPACE_ID") else {
                            return false;
                        };
                        std::path::Path::new(tenant).join(space).join(project)
                    }
                    _ => return false,
                }
            }
        }
        ServiceType::ComputerAgentRunner | ServiceType::ComputerNormalProject => {
            let Some(user) = component("USER_ID").filter(|user| *user == identifier) else {
                return false;
            };
            return workspace_is_persistent(
                inspected,
                &host_root.join(user).to_string_lossy(),
                &target_root,
            );
        }
        ServiceType::Userapp | ServiceType::UserappBuilder => return false,
    };
    workspace_is_persistent(
        inspected,
        &host_root.join(&relative).to_string_lossy(),
        &std::path::Path::new(&target_root)
            .join(relative)
            .to_string_lossy(),
    )
}

fn workspace_is_persistent(
    inspected: &bollard::models::ContainerInspectResponse,
    host: &str,
    target: &str,
) -> bool {
    use std::path::{Component, Path};
    let clean = |p: &str| {
        Path::new(p).is_absolute()
            && !Path::new(p)
                .components()
                .any(|c| matches!(c, Component::ParentDir))
    };
    if !clean(host) || !clean(target) {
        return false;
    }
    let mounts = inspected.mounts.as_deref().unwrap_or_default();
    mounts.iter().any(|mount| {
        let (Some(source), Some(destination)) =
            (mount.source.as_deref(), mount.destination.as_deref())
        else {
            return false;
        };
        // Exact creation-side source and destination; a parent mount is not
        // sufficient proof when a child may shadow the actual workspace.
        clean(source)
            && clean(destination)
            && source == host
            && destination == target
            && mount.rw == Some(true)
            && matches!(mount.typ.as_deref(), Some("bind") | Some("volume"))
            && !mounts.iter().any(|other| {
                other
                    .destination
                    .as_deref()
                    .is_some_and(|d| d != target && Path::new(d).starts_with(target))
            })
    })
}

fn validate_automatic_evidence(
    snapshot: &AgentStopSnapshot,
    current: &bollard::models::ContainerInspectResponse,
) -> Result<()> {
    match snapshot.service_type {
        ServiceType::WebAgentRunner
        | ServiceType::ComputerAgentRunner
        | ServiceType::ComputerNormalProject => {}
        ServiceType::Userapp | ServiceType::UserappBuilder => {
            return Err(Error::Conflict(
                "Application compute requires its lifecycle authority".into(),
            ));
        }
    }
    let expected = snapshot
        .container
        .as_ref()
        .and_then(|c| c.resource_version.as_deref());
    let current = current.state.as_ref().and_then(|s| s.started_at.as_deref());
    if expected.is_none_or(str::is_empty)
        || expected != current
        || !snapshot
            .compute_data_protection
            .as_ref()
            .is_some_and(|p| p.persistent_mounts)
    {
        return Err(Error::Conflict(
            "Docker execution or data protection changed before automatic retirement".into(),
        ));
    }
    Ok(())
}
