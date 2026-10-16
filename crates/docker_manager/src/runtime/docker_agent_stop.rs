//! Captured Agent deletion uses a physical Docker ID throughout the operation.
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use shared_types::ServiceType;

use super::DockerRuntime;

impl DockerRuntime {
    pub(super) async fn stop_captured_agent(
        &self,
        identifier: &str,
        service_type: &ServiceType,
        expected_container_id: &str,
    ) -> Result<()> {
        if expected_container_id.is_empty()
            || !matches!(
                service_type,
                ServiceType::WebAgentRunner
                    | ServiceType::ComputerAgentRunner
                    | ServiceType::ComputerNormalProject
            )
        {
            return Err(Error::Conflict(
                "Captured stop requires an Agent physical identity".into(),
            ));
        }
        let prefix = self
            .inner
            .get_service_config(service_type)
            .await
            .map(|config| config.container_prefix().to_owned())
            .unwrap_or_else(|_| service_type.container_prefix().to_owned());
        let name = crate::utils::DockerUtils::generate_container_name(&prefix, identifier)
            .map_err(Error::ConfigurationError)?;
        let inspect = match self
            .inner
            .get_docker_client()
            .inspect_container(expected_container_id, None)
            .await
        {
            Ok(inspect) => inspect,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => return Ok(()),
            Err(error) => {
                return Err(Error::DockerError(format!(
                    "Inspect captured Agent: {error}"
                )));
            }
        };
        let labels = inspect
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref());
        let family = labels
            .and_then(|labels| labels.get("service-type"))
            .and_then(|value| value.parse::<ServiceType>().ok());
        if inspect.id.as_deref() != Some(expected_container_id)
            || inspect
                .name
                .as_deref()
                .map(|name| name.trim_start_matches('/'))
                != Some(name.as_str())
            || family.is_none_or(|family| !family.same_container_family(*service_type))
            || labels
                .and_then(|labels| labels.get("identifier"))
                .map(String::as_str)
                != Some(identifier)
        {
            return Err(Error::Conflict(
                "Captured Agent identity or ownership changed".into(),
            ));
        }
        // ID-addressed removal cannot select a successor reusing the old name.
        self.inner
            .stop_container_by_id_with_timeout(expected_container_id, 30)
            .await
            .map_err(|error| Error::ContainerStopError(error.to_string()))
    }
}

#[cfg(test)]
mod tests;
