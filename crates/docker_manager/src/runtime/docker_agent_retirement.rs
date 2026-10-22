//! Docker discovery is observational; automatic remnant deletion is unsupported.
use super::DockerRuntime;
use container_runtime_api::{
    AgentResourceIdentity, AgentResourceKind, AgentStopOutcome, AgentStopSnapshot,
    ContainerRuntimeError as Error, ContainerRuntimeResult as Result,
};
use shared_types::ServiceType;
use std::collections::HashMap;

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
        let mut result = Vec::new();
        for container in containers {
            let Some(labels) = container.labels.as_ref() else {
                continue;
            };
            let Some(family) = labels
                .get("service-type")
                .and_then(|value| value.parse::<ServiceType>().ok())
            else {
                continue;
            };
            match family {
                ServiceType::Userapp => continue,
                ServiceType::WebAgentRunner
                | ServiceType::ComputerAgentRunner
                | ServiceType::ComputerNormalProject
                | ServiceType::UserappBuilder => {}
            }
            let Some(identifier) = labels.get("identifier").filter(|value| !value.is_empty())
            else {
                continue;
            };
            let uid = container
                .id
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    Error::DockerError("Managed Agent has no full container ID".into())
                })?;
            let name = container
                .names
                .as_ref()
                .and_then(|names| names.first())
                .map(|value| value.trim_start_matches('/').to_owned())
                .ok_or_else(|| Error::DockerError("Managed Agent has no container name".into()))?;
            let identity = AgentResourceIdentity {
                kind: AgentResourceKind::Container,
                name,
                uid,
                resource_version: None,
                owner_uid: None,
                created_at: container
                    .created
                    .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0)),
            };
            let endpoint = container
                .network_settings
                .as_ref()
                .and_then(|settings| settings.networks.as_ref())
                .and_then(|networks| {
                    networks
                        .values()
                        .find_map(|network| network.ip_address.as_ref().filter(|ip| !ip.is_empty()))
                })
                .map(|ip| format!("http://{ip}:{}", shared_types::GRPC_DEFAULT_PORT));
            result.push(AgentStopSnapshot {
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
            });
        }
        Ok(result)
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
        }))
    }

    pub(super) async fn retire_docker_agent_snapshot(
        &self,
        snapshot: &AgentStopSnapshot,
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
        // Explicit compute retirement only. It is not a writable-layer backup;
        // automatic Docker reclamation remains observation-only.
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
