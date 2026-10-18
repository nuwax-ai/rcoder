//! Canonical, idempotent retirement of the original captured builder registration.
#[cfg(test)]
mod tests;
use super::{database, repo};
use crate::{db::models, pg::PgStore};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use shared_types::BuilderControlTarget;
use std::sync::atomic::Ordering;
use toasty::Executor;
use toasty_core::schema::db::Type;

struct InFlight<'a>(&'a PgStore);
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.active_writes.fetch_sub(1, Ordering::AcqRel);
        self.0.write_finished.notify_waiters();
    }
}

fn receipt_identity(target: &BuilderControlTarget) -> Result<(String, String)> {
    let context = &target.context;
    context
        .validate_identity(&context.app_id)
        .map_err(anyhow::Error::msg)?;
    // Executor epochs authorize a first mutation, but are not the identity of
    // its result. Recovery may observe an already committed original receipt.
    // Routing, RVs, defaults and random runtime snapshot IDs are not included.
    let identity = serde_json::json!({
        "app_id": context.app_id,
        "lifecycle_id": context.lifecycle_id,
        "operation_id": context.operation_id,
        "request_fingerprint": context.request_fingerprint,
        "workload": target.workload.as_ref().map(|w| (&w.kind, &w.name, &w.uid)),
        "pod": target.pod.as_ref().map(|p| (&p.name, &p.uid)),
    });
    let hash = Sha256::digest(serde_json::to_vec(&identity)?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok((
        format!(
            "builder-registry-retirement:{}:{}:{}",
            context.app_id, context.lifecycle_id, context.operation_id
        ),
        hash,
    ))
}

impl PgStore {
    pub async fn capture_builder_registry_identity(
        &self,
        app_id: &str,
    ) -> Result<Option<shared_types::BuilderRegistryIdentity>> {
        let app_id = app_id.to_owned();
        let observation = database::read(&self.database, move |tx| {
            Box::pin(async move {
                let Some(project) = models::Project::filter_by_project_id(&app_id)
                    .first()
                    .exec(tx)
                    .await?
                else {
                    return Ok(None);
                };
                ensure!(
                    historical_family(project.service_type.as_deref())
                        .is_none_or(|family| family == shared_types::ServiceType::UserappBuilder),
                    "Builder registration belongs to another service family"
                );
                let Some(name) = project.container_name else {
                    return Ok(None);
                };
                let container = models::Container::filter_by_container_name(name)
                    .first()
                    .exec(tx)
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!("Current builder container registration cannot be read")
                    })?;
                ensure!(
                    historical_family(Some(&container.service_type))
                        .is_none_or(|family| family == shared_types::ServiceType::UserappBuilder),
                    "Builder container belongs to another service family"
                );
                // These ORM fields are current identity evidence. Do not
                // hydrate unrelated model/status JSON or publish the mirror.
                Ok(container
                    .container_id
                    .filter(|uid| !uid.is_empty())
                    .map(|container_id| shared_types::BuilderRegistryIdentity {
                        generation: project.generation,
                        container_id,
                    }))
            })
        });
        tokio::time::timeout(Self::DURABLE_COMMIT_TIMEOUT, observation).await
            .map_err(|_| anyhow::anyhow!("Canonical builder identity observation exceeded its original durable deadline; no physical deletion was submitted"))?
            .context("Capture canonical builder registry identity")
    }

    pub async fn check_captured_builder_registration(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<()> {
        let target = target.clone();
        let transaction = database::write_with_conflict_retry(&self.database, move |tx| {
            let target = target.clone();
            Box::pin(async move {
                let original = crate::userapp_lifecycle::authorize_builder_registration_retirement(
                    tx, &target,
                )
                .await?;
                lock_registration(tx, &target, None).await?;
                canonical_registration(tx, &target, original.as_ref()).await?;
                Ok(())
            })
        });
        tokio::time::timeout(Self::DURABLE_COMMIT_TIMEOUT, transaction).await
            .map_err(|_| anyhow::anyhow!("Captured builder registration observation exceeded its original durable deadline; no physical deletion was submitted"))?
    }

    pub async fn retire_captured_builder_registration(
        &self,
        target: &BuilderControlTarget,
    ) -> Result<()> {
        let (request, fingerprint) = receipt_identity(target)?;
        {
            let _registration = self
                .registration
                .lock()
                .map_err(|_| anyhow::anyhow!("Project registration lock poisoned"))?;
            ensure!(
                !self.closing.load(Ordering::Acquire),
                "Persistence is shutting down"
            );
            self.active_writes.fetch_add(1, Ordering::AcqRel);
        }
        let _in_flight = InFlight(self);
        let captured = target.clone();
        let transaction = database::write_with_conflict_retry(&self.database, move |tx| {
            let target = captured.clone();
            let request = request.clone();
            let fingerprint = fingerprint.clone();
            Box::pin(async move { retire(tx, &target, &request, &fingerprint).await })
        });
        let retired = tokio::time::timeout(Self::DURABLE_COMMIT_TIMEOUT, transaction).await
            .map_err(|_| shared_types::OperationOutcomeUnknown::new("builder_registry_retirement", "Builder registry retirement outcome is unknown; inspect the original operation receipt"))?
            .context("Retire captured builder registration")?;
        // A committed SQL result is authoritative. Only its exact row may be
        // removed from the mirror; a concurrent/new incarnation stays visible.
        if let Some((generation, physical)) = retired {
            let _registration = self
                .registration
                .lock()
                .map_err(|_| anyhow::anyhow!("Project registration lock poisoned"))?;
            if let Some(physical) = physical {
                self.inner.remove_if_container_identity(
                    &target.context.app_id,
                    &generation,
                    &physical,
                );
            } else {
                self.inner
                    .remove_if_generation(&target.context.app_id, &generation);
            }
        }
        Ok(())
    }
}

async fn retire(
    tx: &mut dyn Executor,
    target: &BuilderControlTarget,
    request: &str,
    fingerprint: &str,
) -> Result<Option<(String, Option<String>)>> {
    // A receipt is immutable. A read-only replay never claims fresh authority
    // and never targets a newer registration after the original commit.
    if let Some(receipt) = models::ProjectWriteReceipt::filter_by_request_id(request)
        .first()
        .exec(tx)
        .await?
    {
        ensure!(
            receipt.fingerprint == fingerprint && receipt.outcome == "committed",
            "Builder retirement request identity was reused"
        );
        return Ok(None);
    }
    // Match builder registration's root -> project/container lock order.
    let original =
        crate::userapp_lifecycle::authorize_builder_registration_retirement(tx, target).await?;
    lock_registration(tx, target, Some(request)).await?;
    if let Some(receipt) = models::ProjectWriteReceipt::filter_by_request_id(request)
        .first()
        .exec(tx)
        .await?
    {
        ensure!(
            receipt.fingerprint == fingerprint && receipt.outcome == "committed",
            "Builder retirement request identity was reused"
        );
        // A replay observes the original result, never a newer same-name row.
        return Ok(None);
    }
    let retired = canonical_registration(tx, target, original.as_ref()).await?;
    if let Some((generation, _)) = &retired {
        ensure!(
            repo::remove_project(tx, &target.context.app_id, generation).await?
                == shared_types::persistence::PersistenceOperationOutcome::Committed,
            "Builder registration changed during conditional retirement"
        );
    }
    models::ProjectWriteReceipt::create()
        .request_id(request)
        .fingerprint(fingerprint)
        .outcome("committed")
        .recorded_at_us(chrono::Utc::now().timestamp_micros())
        .exec(tx)
        .await?;
    Ok(retired)
}

async fn canonical_registration(
    tx: &mut dyn Executor,
    target: &BuilderControlTarget,
    original: Option<&shared_types::BuilderRegistryIdentity>,
) -> Result<Option<(String, Option<String>)>> {
    let actual_uid = target.pod.as_ref().map(|pod| pod.uid.as_str()).or_else(|| {
        target
            .workload
            .as_ref()
            .and_then(|workload| match workload.kind {
                shared_types::AppResourceKind::Container => Some(workload.uid.as_str()),
                shared_types::AppResourceKind::StatefulSet
                | shared_types::AppResourceKind::Deployment
                | shared_types::AppResourceKind::Service
                | shared_types::AppResourceKind::ConfigMap
                | shared_types::AppResourceKind::Secret
                | shared_types::AppResourceKind::HttpRoute
                | shared_types::AppResourceKind::PersistentVolumeClaim
                | shared_types::AppResourceKind::HostPath
                | shared_types::AppResourceKind::File => None,
            })
    });
    let others = toasty::sql::query("SELECT p.project_id FROM projects p LEFT JOIN containers c ON c.container_name=p.container_name WHERE p.project_id<>$1 AND (p.container_name=$2 OR (c.container_id IS NOT NULL AND c.container_id=$3))")
        .bind(&target.context.app_id).bind_typed(target.workload.as_ref().map(|workload| workload.name.clone()), Type::Text).bind_typed(actual_uid.map(str::to_owned), Type::Text).exec(tx).await?;
    ensure!(
        others.is_empty(),
        "Captured physical builder has shared references"
    );
    if let Some(project) = models::Project::filter_by_project_id(&target.context.app_id)
        .first()
        .exec(tx)
        .await?
    {
        // Project payload/model/status JSON is unrelated history. Reading the
        // ORM models directly avoids making its decoding a cleanup gate.
        let container = if let Some(name) = &project.container_name {
            Some(
                models::Container::filter_by_container_name(name)
                    .first()
                    .exec(tx)
                    .await?
                    .ok_or_else(|| {
                        anyhow::anyhow!("Current builder container registration cannot be read")
                    })?,
            )
        } else {
            None
        };
        let family = historical_family(project.service_type.as_deref());
        if let Some(container) = &container {
            ensure!(
                historical_family(Some(&container.service_type))
                    .is_none_or(|family| family == shared_types::ServiceType::UserappBuilder),
                "Builder container belongs to another service family"
            );
        }
        crate::adapter::validate_builder_registration_retirement(
            &project.project_id,
            family,
            container
                .as_ref()
                .map(|container| crate::adapter::BuilderPhysicalRegistration {
                    name: &container.container_name,
                    uid: container
                        .container_id
                        .as_deref()
                        .filter(|uid| !uid.is_empty()),
                    workload_uid: container
                        .workload_uid
                        .as_deref()
                        .filter(|uid| !uid.is_empty()),
                }),
            target,
            original,
        )?;
        if let Some(container) = &container {
            let others = toasty::sql::query("SELECT p.project_id FROM projects p LEFT JOIN containers c ON c.container_name=p.container_name WHERE p.project_id<>$1 AND (p.container_name=$2 OR (c.container_id IS NOT NULL AND c.container_id=$3))")
                .bind(&target.context.app_id).bind(&container.container_name).bind_typed(container.container_id.clone(), Type::Text).exec(tx).await?;
            ensure!(
                others.is_empty(),
                "Captured builder registration has shared references"
            );
        }
        Ok(Some((
            project.generation,
            container.and_then(|container| container.container_id),
        )))
    } else {
        Ok(None)
    }
}

async fn lock_registration(
    tx: &mut dyn Executor,
    target: &BuilderControlTarget,
    request: Option<&str>,
) -> Result<()> {
    let mut keys = std::collections::BTreeSet::from([format!("project:{}", target.context.app_id)]);
    if let Some(request) = request {
        keys.insert(format!("receipt:{request}"));
    }
    if let Some(workload) = &target.workload {
        keys.insert(format!("container-name:{}", workload.name));
        keys.insert(format!(
            "container:{}",
            target
                .pod
                .as_ref()
                .map_or(workload.uid.as_str(), |p| p.uid.as_str())
        ));
    }
    for key in keys {
        toasty::sql::query("SELECT 1 FROM pg_advisory_xact_lock(hashtextextended($1, 719324))")
            .bind(key)
            .exec(tx)
            .await?;
    }
    Ok(())
}

fn historical_family(value: Option<&str>) -> Option<shared_types::ServiceType> {
    value.and_then(|wire| match wire.parse() {
        Ok(family) => Some(family),
        Err(error) => {
            // Exact live capture and the current deletion operation provide
            // ownership; an old unreadable family string does not prove a new
            // execution. Recognized foreign families are rejected by callers.
            tracing::debug!(wire, %error, "Retained unrecognized historical builder family during retirement");
            None
        }
    })
}
