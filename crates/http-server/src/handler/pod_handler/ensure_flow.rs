//! Ordinary ensure delegates all workload observation/creation to the same
//! manager as chat. No elapsed marker or cache miss is deletion authority.
use super::helpers::resolve_resource_limits_from_config;
use super::*;

pub(super) async fn ensure_with_retry(
    state: &Arc<AppState>,
    request: &EnsurePodRequest,
    service_type: &ServiceType,
    container_identifier: &str,
) -> Result<(ContainerBasicInfo, bool), AppError> {
    let options = ContainerCreateOptions {
        user_id: request.user_id.clone(),
        project_id: request.project_id.clone(),
        resource_limits: resolve_resource_limits_from_config(
            state,
            service_type,
            request.resource_limits.clone(),
        )?,
        pod_id: request.pod_id.clone(),
        isolation_type: request.isolation_type.clone(),
        tenant_id: request.tenant_id.clone(),
        space_id: request.space_id.clone(),
        service_type: *service_type,
    };
    let started = Instant::now();
    for attempt in 1..=3 {
        match ComputerContainerManager::ensure_container_for_user_with_type(
            &options,
            state.runtime(),
        )
        .await
        {
            Ok(result) => return Ok(result),
            Err(error) => {
                // Preserve the existing bounded retry for acknowledged create /
                // start failures. Pending waiters, cancellation, conflicts and
                // unknown writes must return without another creation attempt.
                let creation_failed = matches!(&error, AppError::Structured(detail)
                    if matches!(detail.code.as_str(),
                        shared_types::ERR_CONTAINER_CREATE_FAILED |
                        shared_types::ERR_CONTAINER_START_FAILED));
                if attempt == 3 || !creation_failed {
                    return Err(error);
                }
                warn!(
                    %container_identifier, attempt, elapsed = ?started.elapsed(),
                    %error, "Acknowledged container creation failure; retrying observation"
                );
                tokio::time::sleep(std::time::Duration::from_millis(200 * attempt)).await;
            }
        }
    }
    Err(AppError::internal_server_error(
        "Container ensure exhausted its attempts",
    ))
}
