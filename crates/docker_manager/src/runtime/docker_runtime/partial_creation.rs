use super::*;

// Only the final write's explicit rejection, or a read after successful writes,
// proves that no daemon mutation remains outstanding. Never inspect-and-guess
// after a transport failure, timeout, or server error from start.
pub(super) fn acknowledged_partial_creation(error: &crate::DockerError) -> Option<&str> {
    let crate::DockerError::ContainerCreationIncomplete {
        container_id,
        phase,
        source,
    } = error
    else {
        return None;
    };
    match phase {
        crate::ContainerCreationPhase::Observe => Some(container_id),
        crate::ContainerCreationPhase::Start => {
            if let crate::DockerError::BollardError(
                bollard::errors::Error::DockerResponseServerError { status_code, .. },
            ) = source.as_ref()
                && shared_types::RuntimeRequestRejection::from_status(*status_code, String::new())
                    .is_some()
            {
                Some(container_id)
            } else {
                None
            }
        }
    }
}

impl DockerRuntime {
    pub(super) async fn record_partial_creation(
        &self,
        context: &shared_types::UserAppExecutionContext,
        physical_id: &str,
        lease: Option<shared_types::UserAppOperationLeaseReceipt>,
    ) -> ContainerRuntimeResult<()> {
        let target = self
            .capture_builder_compute_with_binding(context, None, false)
            .await?;
        let resource = target
            .workload
            .as_ref()
            .filter(|resource| resource.uid == physical_id)
            .ok_or_else(|| {
                ContainerRuntimeError::Conflict("Partial builder creation identity differs".into())
            })?;
        let inspected = self
            .inner
            .get_docker_client()
            .inspect_container(physical_id, None)
            .await
            .map_err(|error| {
                ContainerRuntimeError::DockerError(format!(
                    "Inspect partial builder creation: {error}"
                ))
            })?;
        if inspected.id.as_deref() != Some(physical_id) {
            return Err(ContainerRuntimeError::Conflict(
                "Partial builder physical ID differs".into(),
            ));
        }
        let created = inspected.created.as_deref().ok_or_else(|| {
            ContainerRuntimeError::Conflict("Partial builder creation timestamp missing".into())
        })?;
        let created_at = chrono::DateTime::parse_from_rfc3339(created)
            .map_err(|error| ContainerRuntimeError::ConfigurationError(error.to_string()))?
            .with_timezone(&chrono::Utc);
        let running = inspected
            .state
            .as_ref()
            .and_then(|state| state.running)
            .ok_or_else(|| {
                ContainerRuntimeError::Conflict("Partial builder running state missing".into())
            })?;
        let network = inspected
            .host_config
            .as_ref()
            .and_then(|config| config.network_mode.as_deref());
        let address = extract_container_ip(&inspected, network);
        let container = ContainerBasicInfo {
            container_id: physical_id.into(),
            container_name: resource.name.clone(),
            container_ip: address.clone(),
            internal_port: shared_types::GRPC_DEFAULT_PORT,
            external_port: 0,
            project_id: context.app_id.clone(),
            status: if running { "Running" } else { "Stopped" }.into(),
            created_at,
            service_url: if address.is_empty() {
                String::new()
            } else {
                format!("http://{address}:{}", shared_types::GRPC_DEFAULT_PORT)
            },
            workload_uid: None,
        };
        let after = self
            .capture_builder_compute_with_binding(context, None, false)
            .await?;
        if target != after {
            return Err(ContainerRuntimeError::Conflict(
                "Partial builder changed during observation".into(),
            ));
        }
        let receipt = crate::runtime::builder_creation_receipt::BuilderCreationReceipt {
            target,
            container,
            lease: lease.ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError("Partial builder lease missing".into())
            })?,
        };
        crate::runtime::docker_compute_receipt::save_creation(&receipt).await
    }
}
