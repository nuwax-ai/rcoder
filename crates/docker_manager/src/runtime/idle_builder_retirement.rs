//! Idle retirement is scoped to the physical compute captured before the idle
//! decision. It never resolves a stable name into replacement deletion authority.

use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use shared_types::ContainerBasicInfo;
use std::sync::atomic::{AtomicBool, Ordering};

use super::docker_runtime::DockerRuntime;

// Preserve the prior 8s management drain + 30s physical convergence budget,
// while including all target reads instead of resetting a budget per call.
pub(super) const RETIREMENT_BUDGET: std::time::Duration = std::time::Duration::from_secs(38);

pub(super) fn deadline_error(dispatched: bool) -> Error {
    Error::Timeout(if dispatched {
        "Captured idle builder stop outcome is unknown after the shared retirement deadline".into()
    } else {
        "Captured idle builder observation timed out before stop dispatch".into()
    })
}

impl DockerRuntime {
    pub(super) async fn retire_captured_idle_builder(
        &self,
        identifier: &str,
        captured: &ContainerBasicInfo,
    ) -> Result<()> {
        self.retire_captured_idle_builder_until(
            identifier,
            captured,
            tokio::time::Instant::now() + RETIREMENT_BUDGET,
        )
        .await
    }

    pub(super) async fn retire_captured_idle_builder_until(
        &self,
        identifier: &str,
        captured: &ContainerBasicInfo,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let dispatched = AtomicBool::new(false);
        tokio::time::timeout_at(
            deadline,
            self.retire_captured_idle_builder_inner(identifier, captured, &dispatched),
        )
        .await
        .map_err(|_| deadline_error(dispatched.load(Ordering::Acquire)))?
    }

    async fn retire_captured_idle_builder_inner(
        &self,
        identifier: &str,
        captured: &ContainerBasicInfo,
        dispatched: &AtomicBool,
    ) -> Result<()> {
        let name = &captured.container_name;
        if captured.project_id != identifier
            || identifier.is_empty()
            || name.is_empty()
            || captured.container_id.is_empty()
            || captured.workload_uid.is_some()
        {
            return Err(Error::Conflict(
                "Captured idle builder identity mismatch".into(),
            ));
        }
        let client = self.inner.get_docker_client();
        let info = match client.inspect_container(&captured.container_id, None).await {
            Ok(info) => info,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                return self
                    .inner
                    .retire_container_cache(&captured.container_id)
                    .await
                    .map_err(|error| {
                        Error::DockerError(format!("Retire absent captured builder cache: {error}"))
                    });
            }
            Err(error) => {
                return Err(Error::DockerError(format!(
                    "Inspect captured idle builder: {error}"
                )));
            }
        };
        if info
            .name
            .as_deref()
            .map(|value| value.trim_start_matches('/'))
            != Some(name.as_str())
            || super::docker_builder_deletion::builder_identity(info, name, identifier)?.uid
                != captured.container_id
        {
            return Err(Error::Conflict("Captured idle builder was replaced".into()));
        }
        dispatched.store(true, Ordering::Release);
        let drain = super::docker_app_runtime::execute_container_command(
            client,
            &captured.container_id,
            vec![
                "app-cli".into(),
                "--app-cli-container-stop".into(),
                format!("/home/user/{identifier}"),
            ],
        );
        let result = tokio::time::timeout(std::time::Duration::from_secs(8), drain).await;
        if !matches!(result, Ok(Ok(exit)) if exit.exit_code == 0) {
            tracing::warn!(%identifier, "Captured idle owner drain incomplete; proceeding with exact compute retirement");
        }
        // Docker's immutable full ID remains the only target after drain. v=false
        // preserves anonymous volumes too; bind mounts and named data never become
        // cleanup targets, even when management is unavailable.
        let removed = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            client.remove_container(
                &captured.container_id,
                Some(bollard::query_parameters::RemoveContainerOptions {
                    force: true,
                    v: false,
                    link: false,
                }),
            ),
        )
        .await
        .map_err(|_| Error::Timeout("Captured idle builder removal outcome is unknown".into()))?;
        match removed {
            Ok(())
            | Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {}
            Err(error) => {
                return Err(Error::DockerError(format!(
                    "Remove captured idle builder: {error}"
                )));
            }
        }
        match client.inspect_container(&captured.container_id, None).await {
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => self
                .inner
                .retire_container_cache(&captured.container_id)
                .await
                .map_err(|error| {
                    Error::DockerError(format!("Retire captured builder cache: {error}"))
                }),
            Ok(_) => Err(Error::Conflict(
                "Captured idle builder remains present after removal".into(),
            )),
            Err(error) => Err(Error::DockerError(format!(
                "Confirm captured idle builder removal: {error}"
            ))),
        }
    }
}

#[cfg(test)]
mod deadline_tests;
#[cfg(test)]
mod tests;
