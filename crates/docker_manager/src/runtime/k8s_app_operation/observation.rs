//! Read-only wake hints for the production application mutex. No lease handle
//! or mutation primitive is used here; an observation never grants authority.

use super::*;
use container_runtime_api::{AppOperationMutexIdentity, AppOperationMutexObservation};
use futures_util::TryStreamExt;
use kube::api::{WatchEvent, WatchParams};

#[cfg(test)]
#[path = "observation_tests.rs"]
mod tests;

fn validate_request(app_id: &str, lifecycle_id: &str) -> ContainerRuntimeResult<()> {
    if app_id.is_empty() || lifecycle_id.is_empty() {
        return Err(ContainerRuntimeError::ConfigurationError(
            "Application mutex observation requires application and lifecycle IDs".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_observed_scope(
    observed: &AppOperationMutexIdentity,
    namespace: &str,
    name: &str,
    _lifecycle_id: &str,
) -> ContainerRuntimeResult<()> {
    if observed.namespace != namespace
        || observed.name != name
        || observed.uid.is_empty()
        || observed.token.is_empty()
        || observed.resource_version.is_empty()
        || observed.lifecycle_id.as_deref().is_some_and(str::is_empty)
    {
        return Err(ContainerRuntimeError::Conflict(
            "Observed application mutex scope or identity differs".into(),
        ));
    }
    Ok(())
}

/// Validate every object from GET and watch alike. The token is never included
/// in diagnostics. Lifecycle-less legacy Leases remain visible as occupied,
/// and this type supplies no path from that observation to write authority.
pub(super) fn mutex_identity(
    object: &Lease,
    namespace: &str,
    name: &str,
    app_id: &str,
    _lifecycle_id: &str,
) -> ContainerRuntimeResult<AppOperationMutexIdentity> {
    let labels = object.metadata.labels.as_ref();
    let observed_lifecycle = object
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get("rcoder.io/lifecycle-id"))
        .filter(|lifecycle_id| !lifecycle_id.is_empty())
        .cloned();
    if object.metadata.namespace.as_deref() != Some(namespace)
        || object.metadata.name.as_deref() != Some(name)
        || labels
            .and_then(|labels| labels.get("rcoder.io/operation-app"))
            .map(String::as_str)
            != Some(app_id)
        || labels
            .and_then(|labels| labels.get("rcoder.io/operation-family"))
            .map(String::as_str)
            != Some("user-app")
        || observed_lifecycle.as_deref().is_some_and(str::is_empty)
    {
        return Err(ContainerRuntimeError::Conflict(
            "Application mutex scope or holder identity is invalid".into(),
        ));
    }
    let required = |value: Option<&str>, field: &str| {
        value
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                ContainerRuntimeError::Conflict(format!("Application mutex {field} is missing"))
            })
    };
    Ok(AppOperationMutexIdentity {
        namespace: namespace.to_owned(),
        name: name.to_owned(),
        uid: required(object.metadata.uid.as_deref(), "UID")?,
        token: holder_token(object).ok_or_else(|| {
            ContainerRuntimeError::Conflict("Application mutex holder token is missing".into())
        })?,
        resource_version: required(
            object.metadata.resource_version.as_deref(),
            "resourceVersion",
        )?,
        lifecycle_id: observed_lifecycle,
    })
}

/// Expiry itself does not emit a Kubernetes watch event. Wake for a fresh GET
/// when the original typed renewal can first satisfy `lease_expired_at` (whose
/// whole-second age must be strictly greater than TTL). This local timer never
/// authorizes acquisition, and the caller's total deadline remains unchanged.
fn expiry_recheck_after(object: &Lease) -> std::time::Duration {
    let Some(spec) = object.spec.as_ref() else {
        return std::time::Duration::ZERO;
    };
    let Some(renew) = spec.renew_time.as_ref() else {
        return std::time::Duration::ZERO;
    };
    let ttl = i64::from(spec.lease_duration_seconds.unwrap_or(LEASE_TTL_SECONDS));
    let expiry = i128::from(renew.0.as_microsecond()) + (i128::from(ttl) + 1) * 1_000_000;
    let now = i128::from(k8s_openapi::jiff::Timestamp::now().as_microsecond());
    std::time::Duration::from_micros((expiry - now).clamp(1, i128::from(u64::MAX)) as u64)
}

fn mutex_observation(
    object: &Lease,
    namespace: &str,
    name: &str,
    app_id: &str,
    lifecycle_id: &str,
) -> ContainerRuntimeResult<AppOperationMutexObservation> {
    let identity = mutex_identity(object, namespace, name, app_id, lifecycle_id)?;
    if object.metadata.deletion_timestamp.is_some() {
        return Err(ContainerRuntimeError::Conflict(
            "Application mutex is deleting; expiry cannot authorize acquisition".into(),
        ));
    }
    Ok(
        if lease_expired_at(k8s_openapi::jiff::Timestamp::now(), object) {
            AppOperationMutexObservation::Expired(identity)
        } else {
            AppOperationMutexObservation::Occupied(identity)
        },
    )
}

impl KubernetesRuntime {
    pub(crate) async fn observe_application_mutex(
        &self,
        app_id: &str,
        lifecycle_id: &str,
    ) -> ContainerRuntimeResult<AppOperationMutexObservation> {
        validate_request(app_id, lifecycle_id)?;
        let name = operation_name(app_id, &ServiceType::Userapp)?;
        self.read_application_mutex(app_id, lifecycle_id, &name)
            .await
            .map(|(observation, _)| observation)
    }

    async fn read_application_mutex(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        name: &str,
    ) -> ContainerRuntimeResult<(AppOperationMutexObservation, std::time::Duration)> {
        let api: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
        if let Some(object) = api.get_opt(name).await.map_err(|error| {
            ContainerRuntimeError::K8sError(format!("Read application operation mutex: {error}"))
        })? {
            let observation =
                mutex_observation(&object, &self.namespace, name, app_id, lifecycle_id)?;
            return Ok((observation, expiry_recheck_after(&object)));
        }
        // This observes only the native Lease. Legacy ConfigMaps are retained
        // diagnostics, not a new presence-only startup gate. The existing PG
        // admission remains mandatory after vacant-only native acquisition.
        Ok((
            AppOperationMutexObservation::Absent,
            std::time::Duration::ZERO,
        ))
    }

    /// After 410, a GET of an unchanged Lease can keep its compacted object RV.
    /// Recover the watch cursor from an exact-name collection snapshot instead;
    /// its items retain their own RV for identity and conditional acquisition.
    async fn relist_application_mutex(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        name: &str,
    ) -> ContainerRuntimeResult<(AppOperationMutexObservation, std::time::Duration, String)> {
        let api: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
        let list = api
            .list(&ListParams::default().fields(&format!("metadata.name={name}")))
            .await
            .map_err(|error| {
                ContainerRuntimeError::K8sError(format!(
                    "Relist application operation mutex after expired watch: {error}"
                ))
            })?;
        if list.items.len() > 1
            || list
                .metadata
                .continue_
                .as_deref()
                .is_some_and(|value| !value.is_empty())
        {
            return Err(ContainerRuntimeError::Conflict(
                "Exact application mutex relist was ambiguous or incomplete".into(),
            ));
        }
        let cursor = list
            .metadata
            .resource_version
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ContainerRuntimeError::K8sError(
                    "Application mutex relist has no collection resourceVersion".into(),
                )
            })?;
        let (observation, recheck_after) = match list.items.into_iter().next() {
            Some(object) => (
                mutex_observation(&object, &self.namespace, name, app_id, lifecycle_id)?,
                expiry_recheck_after(&object),
            ),
            // Empty LIST is only a wake hint. Preserve the exact-name GET
            // absence contract; neither observation grants write authority.
            None => {
                self.read_application_mutex(app_id, lifecycle_id, name)
                    .await?
            }
        };
        Ok((observation, recheck_after, cursor))
    }

    pub(crate) async fn wait_application_mutex_change(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        observed: &AppOperationMutexIdentity,
        budget: std::time::Duration,
    ) -> ContainerRuntimeResult<AppOperationMutexObservation> {
        validate_request(app_id, lifecycle_id)?;
        let name = operation_name(app_id, &ServiceType::Userapp)?;
        validate_observed_scope(observed, &self.namespace, &name, lifecycle_id)?;
        if budget.is_zero() {
            return Ok(AppOperationMutexObservation::Unconfirmed);
        }
        let deadline = tokio::time::Instant::now()
            .checked_add(budget)
            .ok_or_else(|| {
                ContainerRuntimeError::ConfigurationError(
                    "Application mutex wait budget is too large".into(),
                )
            })?;
        // The one outer deadline covers initial/follow-up GETs, watch setup,
        // stream reads, EOF reconnects, and every 410 recovery without reset.
        match tokio::time::timeout_at(
            deadline,
            self.wait_application_mutex_until(app_id, lifecycle_id, &name, observed, deadline),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Ok(AppOperationMutexObservation::Unconfirmed),
        }
    }

    async fn wait_application_mutex_until(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        name: &str,
        observed: &AppOperationMutexIdentity,
        deadline: tokio::time::Instant,
    ) -> ContainerRuntimeResult<AppOperationMutexObservation> {
        let api: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
        let mut relist_after_gone = false;
        let mut watch_cursor = None;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Ok(AppOperationMutexObservation::Unconfirmed);
            }
            let (observation, recheck_after) = if relist_after_gone {
                let (observation, recheck_after, cursor) = self
                    .relist_application_mutex(app_id, lifecycle_id, name)
                    .await?;
                relist_after_gone = false;
                watch_cursor = Some(cursor);
                (observation, recheck_after)
            } else {
                self.read_application_mutex(app_id, lifecycle_id, name)
                    .await?
            };
            let current = match observation {
                AppOperationMutexObservation::Absent => {
                    return Ok(AppOperationMutexObservation::Absent);
                }
                AppOperationMutexObservation::Unconfirmed => {
                    return Ok(AppOperationMutexObservation::Unconfirmed);
                }
                AppOperationMutexObservation::Expired(identity) => {
                    return Ok(AppOperationMutexObservation::Expired(identity));
                }
                AppOperationMutexObservation::Occupied(identity) => identity,
            };
            if &current != observed {
                return Ok(AppOperationMutexObservation::Occupied(current));
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(AppOperationMutexObservation::Unconfirmed);
            }
            let recheck_at = tokio::time::Instant::now()
                .checked_add(recheck_after.min(remaining))
                .unwrap_or(deadline)
                .min(deadline);
            // API timeout is only an upper bound per stream; the outer deadline
            // enforces subsecond budgets and budgets longer than one stream.
            let params = WatchParams::default()
                .fields(&format!("metadata.name={name}"))
                .timeout(remaining.as_secs().clamp(1, 294) as u32);
            let stream = match tokio::time::timeout_at(
                recheck_at,
                api.watch(
                    &params,
                    watch_cursor.as_deref().unwrap_or(&current.resource_version),
                ),
            )
            .await
            {
                Ok(Ok(stream)) => stream,
                Ok(Err(kube::Error::Api(status))) if status.code == 410 => {
                    relist_after_gone = true;
                    continue;
                }
                Err(_) => continue,
                Ok(Err(error)) => {
                    return Err(ContainerRuntimeError::K8sError(format!(
                        "Watch application operation mutex: {error}"
                    )));
                }
            };
            let mut stream = Box::pin(stream);
            loop {
                let event = match tokio::time::timeout_at(recheck_at, stream.try_next()).await {
                    Ok(Ok(Some(event))) => event,
                    Ok(Ok(None)) | Err(_) => break,
                    Ok(Err(kube::Error::Api(status))) if status.code == 410 => {
                        relist_after_gone = true;
                        break;
                    }
                    Ok(Err(error)) => {
                        return Err(ContainerRuntimeError::K8sError(format!(
                            "Read application operation mutex watch: {error}"
                        )));
                    }
                };
                match event {
                    WatchEvent::Added(object) | WatchEvent::Modified(object) => {
                        let observation = mutex_observation(
                            &object,
                            &self.namespace,
                            name,
                            app_id,
                            lifecycle_id,
                        )?;
                        match observation {
                            AppOperationMutexObservation::Occupied(identity) => {
                                if &identity != observed {
                                    return Ok(AppOperationMutexObservation::Occupied(identity));
                                }
                            }
                            // A watch event is only a hint. Fresh GET must
                            // confirm expiry, just as it confirms deletion.
                            AppOperationMutexObservation::Expired(_)
                            | AppOperationMutexObservation::Absent
                            | AppOperationMutexObservation::Unconfirmed => break,
                        }
                    }
                    WatchEvent::Deleted(object) => {
                        mutex_identity(&object, &self.namespace, name, app_id, lifecycle_id)?;
                        // A delete notification is only a wake hint: re-GET
                        // the exact native name before returning Lease absence.
                        break;
                    }
                    WatchEvent::Bookmark(_) => {}
                    WatchEvent::Error(status) => {
                        if status.code == 410 {
                            relist_after_gone = true;
                            break;
                        }
                        return Err(ContainerRuntimeError::K8sError(format!(
                            "Application mutex watch returned status {}",
                            status.code
                        )));
                    }
                }
            }
        }
    }
}
