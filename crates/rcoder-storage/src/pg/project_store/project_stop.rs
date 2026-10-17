//! A clear-all retires its project registration, including unknown SID aliases.
//! The immutable source/successor fence also makes late Stop replays harmless.
use super::persist_ops::PersistOp;
use super::{load, persist_ops::ProjectSnapshot, repo};
use crate::pg::PgStore;
use shared_types::persistence::PersistenceWriteOutcome;
use shared_types::{ProjectAndContainerInfo, persistence::PersistenceOperationOutcome};
use std::collections::HashMap;
use std::sync::Arc;
use toasty::Executor;
use tokio::time::Instant;

#[derive(Clone)]
pub(crate) struct ProjectStop {
    pub expected: Box<ProjectAndContainerInfo>,
    pub successor_generation: String,
}

impl std::fmt::Debug for ProjectStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectStop")
            .field("project_id", &self.expected.project_id())
            .field(
                "source_generation",
                &self.expected.persistence_identity().generation,
            )
            .field("successor_generation", &self.successor_generation)
            .finish()
    }
}

impl ProjectStop {
    pub(super) fn capture(expected: &ProjectAndContainerInfo) -> anyhow::Result<Self> {
        let basic = expected
            .container_info()
            .ok_or_else(|| anyhow::anyhow!("Stop requires a registered physical container"))?;
        anyhow::ensure!(
            !basic.container_id.is_empty(),
            "Stop physical identity is empty"
        );
        anyhow::ensure!(
            expected
                .persistence_identity()
                .container
                .as_ref()
                .is_some_and(|identity| identity.physical_uid.as_deref()
                    == Some(basic.container_id.as_str())),
            "Stop physical identity differs from its registration"
        );
        let mut frozen = expected.clone();
        frozen.set_container(Some(basic));
        Ok(Self {
            expected: Box::new(frozen),
            successor_generation: uuid::Uuid::new_v4().to_string(),
        })
    }

    pub(super) fn successor(&self, current: &ProjectAndContainerInfo) -> ProjectAndContainerInfo {
        let mut successor = current.clone();
        successor.clear_all_sessions();
        let mut identity = successor.persistence_identity().clone();
        identity.predecessor = Some(self.expected.persistence_identity().generation.clone());
        identity.generation = self.successor_generation.clone();
        // This is an unconfirmed cache projection. New metadata must resolve
        // the typed Stop before admission; only Stop commits this epoch.
        identity.revision = 0;
        successor.set_persistence_identity(identity);
        successor
    }
}

/// Exact scope and physical instance, allowing only the project epoch to differ.
pub(super) fn same_stop_scope(
    left: &ProjectAndContainerInfo,
    right: &ProjectAndContainerInfo,
) -> bool {
    let mut adjusted = left.clone();
    let mut identity = adjusted.persistence_identity().clone();
    identity.generation = right.persistence_identity().generation.clone();
    adjusted.set_persistence_identity(identity);
    crate::adapter::session_container_matches(&adjusted, right)
        && left.tenant_id() == right.tenant_id()
        && left.space_id() == right.space_id()
        && left.isolation_type() == right.isolation_type()
}

/// A new request may converge a stale registration after a real clear-all.
/// This is never used by an already dispatched completion or queued command.
pub(super) async fn may_rebase_stop_successor(
    tx: &mut dyn Executor,
    previous: &ProjectAndContainerInfo,
    persisted: &ProjectAndContainerInfo,
) -> anyhow::Result<bool> {
    if !same_stop_scope(previous, persisted) {
        return Ok(false);
    }
    if repo::project_generation_is_retired(
        tx,
        previous.project_id(),
        &previous.persistence_identity().generation,
    )
    .await?
    {
        return Ok(true);
    }
    if let Some(source) = previous.persistence_identity().predecessor.as_deref() {
        return repo::project_generation_is_retired(tx, previous.project_id(), source).await;
    }
    Ok(false)
}

pub(super) async fn execute_stop(
    tx: &mut dyn Executor,
    stop: &ProjectStop,
) -> anyhow::Result<PersistenceOperationOutcome> {
    let Some((row, container)) =
        repo::fetch_project_registration(tx, stop.expected.project_id()).await?
    else {
        return Ok(PersistenceOperationOutcome::Superseded);
    };
    let containers = container
        .map(|row| HashMap::from([(row.container_name.clone(), row)]))
        .unwrap_or_default();
    let current = load::hydrate_project(&row, &containers)?;
    if !same_stop_scope(&stop.expected, &current) {
        return Ok(PersistenceOperationOutcome::Superseded);
    }
    let basic = current
        .container_info()
        .ok_or_else(|| anyhow::anyhow!("Stop container disappeared"))?;
    let container_identity = current
        .persistence_identity()
        .container
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Stop container registration disappeared"))?;
    let retired = toasty::sql::query(
        "SELECT container_name FROM container_tombstones WHERE
           (container_name=$1 AND container_generation=$2) OR physical_uid=$3",
    )
    .bind(&basic.container_name)
    .bind(&container_identity.generation)
    .bind(&basic.container_id)
    .exec(tx)
    .await?;
    if !retired.is_empty()
        || repo::project_generation_is_retired(tx, current.project_id(), &row.generation).await?
    {
        return Ok(PersistenceOperationOutcome::Superseded);
    }
    if row.generation == stop.successor_generation {
        // This exact Stop already committed. Its fallback must not clear later
        // sessions or overwrite metadata admitted on the confirmed epoch.
        return Ok(PersistenceOperationOutcome::Committed);
    }
    if row.generation != stop.expected.persistence_identity().generation {
        return Ok(PersistenceOperationOutcome::Superseded);
    }
    if !crate::adapter::stop_frame_matches(&stop.expected, &current) {
        return Ok(PersistenceOperationOutcome::Superseded);
    }
    // Read metadata under the same writer locks as the transition. A captured
    // older request/model must never replace an update committed before Stop.
    let mut successor = stop.successor(&current);
    let mut identity = successor.persistence_identity().clone();
    identity.revision = 1;
    identity.retired_sessions.clear();
    successor.set_persistence_identity(identity);
    repo::upsert_project(tx, &ProjectSnapshot::from_info(&successor)?).await
}

impl PgStore {
    /// Resolve only this process's provisional Stop projection. Immutable SQL
    /// commands never rebase; this cache is removed once SQL decides their fate.
    pub(super) async fn resolve_project_stop(&self, project_id: &str) -> anyhow::Result<bool> {
        let stop = self
            .project_stops
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(project_id)
            .cloned();
        let Some(stop) = stop else {
            return Ok(true);
        };
        let checked = Arc::clone(&stop);
        let resolved =
            super::database::read(&self.database, move |tx| {
                Box::pin(async move {
                    let Some((row, container)) =
                        repo::fetch_project_registration(tx, checked.expected.project_id()).await?
                    else {
                        return Ok(None);
                    };
                    let containers = container
                        .map(|row| HashMap::from([(row.container_name.clone(), row)]))
                        .unwrap_or_default();
                    let persisted = load::hydrate_project(&row, &containers)?;
                    if !same_stop_scope(&checked.expected, &persisted) {
                        return Ok(Some((persisted, false)));
                    }
                    let own_successor = row.generation == checked.successor_generation;
                    let rejected_frame = row.generation
                        == checked.expected.persistence_identity().generation
                        && !crate::adapter::stop_frame_matches(&checked.expected, &persisted);
                    let source_retired = repo::project_generation_is_retired(
                        tx,
                        checked.expected.project_id(),
                        &checked.expected.persistence_identity().generation,
                    )
                    .await?;
                    Ok((own_successor || rejected_frame || source_retired)
                        .then_some((persisted, true)))
                })
            })
            .await?;
        let Some((persisted, same_scope)) = resolved else {
            return Ok(false);
        };
        let mut registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
        let mut pending = self.project_stops.lock().unwrap_or_else(|p| p.into_inner());
        if !pending
            .get(project_id)
            .is_some_and(|current| Arc::ptr_eq(current, &stop))
        {
            return Ok(true);
        }
        if let Some(current) = self.inner.get(project_id)
            && current.persistence_identity().generation == stop.successor_generation
            && same_stop_scope(&stop.expected, &current)
        {
            if same_scope {
                // No metadata request is admitted on this unconfirmed epoch.
                // Publish the exact SQL result, retaining its current metadata.
                self.inner.insert_with_session(
                    project_id.to_owned(),
                    Arc::new(persisted.clone()),
                    None,
                )?;
            } else {
                // A true physical replacement is never adopted by Stop. Undo
                // only this provisional projection; normal verified recovery
                // can then handle the captured source on a fresh request.
                self.inner
                    .insert(project_id.to_owned(), Arc::new((*stop.expected).clone()))?;
            }
        }
        if persisted.persistence_identity().generation
            == stop.expected.persistence_identity().generation
            && registration.get(project_id)
                == Some(&stop.expected.persistence_identity().generation)
        {
            registration.remove(project_id);
        }
        pending.remove(project_id);
        Ok(true)
    }

    /// Capture before the runner Stop RPC. Only a fresh operation may refresh
    /// its source; a delayed clear/fallback always retains the supplied frame.
    pub async fn capture_project_stop_registration(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Option<Arc<ProjectAndContainerInfo>>> {
        tokio::time::timeout(Self::DURABLE_COMMIT_TIMEOUT, async {
            anyhow::ensure!(
                self.resolve_project_stop(project_id).await?,
                "Project Stop is awaiting durable confirmation"
            );
            let baseline = self.inner.get(project_id);
            let id = project_id.to_owned();
            let previous = baseline.clone();
            let fetched = super::database::read(&self.database, move |tx| {
                Box::pin(async move {
                    let Some((row, container)) = repo::fetch_project_registration(tx, &id).await?
                    else {
                        return Ok(None);
                    };
                    let containers = container
                        .map(|row| HashMap::from([(row.container_name.clone(), row)]))
                        .unwrap_or_default();
                    let persisted = load::hydrate_project(&row, &containers)?;
                    if let Some(previous) = previous {
                        anyhow::ensure!(
                            same_stop_scope(&previous, &persisted),
                            "Stop runtime or ownership changed before dispatch"
                        );
                        anyhow::ensure!(
                            previous.persistence_identity().generation == row.generation
                                || may_rebase_stop_successor(tx, &previous, &persisted).await?,
                            "Stop project successor is not proven"
                        );
                        anyhow::ensure!(
                            previous.persistence_identity().generation != row.generation
                                || previous.persistence_identity().revision <= row.row_revision,
                            "Stop metadata is awaiting durable confirmation"
                        );
                    }
                    Ok(Some(Arc::new(persisted)))
                })
            })
            .await?;
            let Some(fetched) = fetched else {
                return Ok(None);
            };
            let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            let current = self.inner.get(project_id);
            anyhow::ensure!(
                match (baseline.as_ref(), current.as_ref()) {
                    (None, None) => true,
                    (Some(before), Some(current)) => Arc::ptr_eq(before, current),
                    _ => false,
                },
                "Stop metadata changed during source capture"
            );
            if let Some(current) = current
                && crate::adapter::stop_frame_matches(&fetched, &current)
            {
                // Preserve accepted optimistic routing until the actual runner
                // Stop has replied; only the SQL frame is captured here.
                return Ok(Some(current));
            }
            self.inner
                .insert_with_session(project_id.to_owned(), Arc::clone(&fetched), None)?;
            Ok(Some(fetched))
        })
        .await
        .map_err(|_| anyhow::anyhow!("Stop source lookup exceeded the durable deadline"))?
    }

    pub async fn clear_sessions_if_current_durable(
        &self,
        expected: &ProjectAndContainerInfo,
    ) -> anyhow::Result<PersistenceWriteOutcome> {
        let deadline = Instant::now() + Self::DURABLE_COMMIT_TIMEOUT;
        let stop = Arc::new(ProjectStop::capture(expected)?);
        let pending = {
            let mut registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            anyhow::ensure!(
                !self.closing.load(std::sync::atomic::Ordering::Acquire),
                "Persistence is shutting down"
            );
            let Some(current) = self.inner.get(expected.project_id()) else {
                return Ok(PersistenceWriteOutcome::Superseded);
            };
            if !crate::adapter::stop_frame_matches(expected, &current)
                || self
                    .project_stops
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .contains_key(expected.project_id())
            {
                return Ok(PersistenceWriteOutcome::Superseded);
            }
            let successor = stop.successor(&current);
            self.inner.clear_session(expected.project_id());
            self.inner
                .insert(expected.project_id().to_owned(), Arc::new(successor))?;
            registration.insert(
                expected.project_id().to_owned(),
                expected.persistence_identity().generation.clone(),
            );
            self.project_stops
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(expected.project_id().to_owned(), Arc::clone(&stop));
            self.register_durable(vec![PersistOp::StopProject(Box::new((*stop).clone()))])
        };
        let outcome = self
            .execute_durable_before(pending, "stop_project", deadline)
            .await;
        // Readback shares the same deadline. A later fresh admission also runs
        // this resolver if timeout/cancellation left a provisional projection.
        if let Ok(Ok(true)) =
            tokio::time::timeout_at(deadline, self.resolve_project_stop(expected.project_id()))
                .await
            && let Some(current) = self.inner.get(expected.project_id())
            && current.persistence_identity().generation == stop.successor_generation
        {
            return Ok(PersistenceWriteOutcome::Committed);
        }
        Ok(outcome)
    }
}
