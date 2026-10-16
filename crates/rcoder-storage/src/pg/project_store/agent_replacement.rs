//! Registration-only recovery after the runtime proves an owned replacement.
//! Existing generations and tombstones fence delayed old requests; no resource
//! deletion, data-volume mutation, or additional recovery journal is involved.
use super::{
    load,
    persist_ops::{ContainerSnapshot, ProjectSnapshot},
    repo,
};
use crate::{db::models, pg::PgStore};
use anyhow::{Context, Result, ensure};
use shared_types::{
    ProjectAndContainerInfo, VerifiedAgentReplacement,
    persistence::{
        ContainerPersistenceIdentity, PersistenceOperationOutcome, PersistenceWriteOutcome,
        ProjectPersistenceIdentity,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, atomic::Ordering},
};
use toasty::Executor;

struct ReboundRegistration {
    project: ProjectAndContainerInfo,
    project_service_type: String,
    /// Original session generations identify accepted local clears precisely.
    source_sessions: Option<BTreeMap<String, String>>,
    retired_local_sessions: BTreeMap<String, String>,
}

struct InFlight<'a>(&'a PgStore);
impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.active_writes.fetch_sub(1, Ordering::AcqRel);
        self.0.pending_ops.fetch_sub(1, Ordering::AcqRel);
        self.0.write_finished.notify_waiters();
    }
}

impl PgStore {
    pub async fn replace_agent_container_registration_durable(
        &self,
        expected: &ProjectAndContainerInfo,
        evidence: &VerifiedAgentReplacement,
    ) -> Result<PersistenceWriteOutcome> {
        crate::adapter::validate_agent_replacement(expected, evidence)?;
        let baseline = {
            let registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            ensure!(
                !self.closing.load(Ordering::Acquire),
                "Persistence is shutting down"
            );
            if registration.get(expected.project_id())
                == Some(&expected.persistence_identity().generation)
            {
                return Ok(PersistenceWriteOutcome::Superseded);
            }
            let Some(current) = self.inner.get(expected.project_id()) else {
                return Ok(PersistenceWriteOutcome::Superseded);
            };
            self.active_writes.fetch_add(1, Ordering::AcqRel);
            self.pending_ops.fetch_add(1, Ordering::AcqRel);
            current
        };
        let _in_flight = InFlight(self);
        let expected_owned = expected.clone();
        let mut admitted = (*baseline).clone();
        admitted.set_container(baseline.container_info());
        let evidence = evidence.clone();
        let committed = super::database::write_with_conflict_retry(&self.database, move |tx| {
            let expected = expected_owned.clone();
            let admitted = admitted.clone();
            let evidence = evidence.clone();
            Box::pin(async move { replace_rows(tx, &expected, &admitted, &evidence).await })
        })
        .await
        .context("Commit verified Agent registration replacement")?;
        let Some(rebound) = committed else {
            return Ok(PersistenceWriteOutcome::Superseded);
        };
        let committed = rebound.project;
        let mut registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
        let current = self.inner.get(expected.project_id());
        if rebound.source_sessions.is_some()
            && registration.get(expected.project_id())
                == Some(&expected.persistence_identity().generation)
            && current.as_ref().is_none_or(|current| {
                current.persistence_identity().generation
                    == expected.persistence_identity().generation
            })
        {
            // Only a retirement accepted against this operation's still-current
            // source can supersede its newly committed projection. An arbitrary
            // delayed SQL delete never enters this local admission registry.
            let generation = committed.persistence_identity().generation.clone();
            registration.insert(expected.project_id().to_owned(), generation.clone());
            self.enqueue_structural(super::persist_ops::PersistOp::RemoveProject {
                project_id: expected.project_id().to_owned(),
                generation,
            });
            return Ok(PersistenceWriteOutcome::Superseded);
        }
        let Some(current) = current else {
            return Ok(PersistenceWriteOutcome::Committed);
        };
        if registration.get(expected.project_id())
            == Some(&current.persistence_identity().generation)
        {
            return Ok(PersistenceWriteOutcome::Committed);
        }
        // A newer physical generation wins. Ordinary metadata accepted while
        // this transaction was running is merged and registered on the new one.
        let merged_local = !Arc::ptr_eq(&baseline, &current)
            && rebound.source_sessions.is_some()
            && current.persistence_identity().generation
                == expected.persistence_identity().generation
            && (current.persistence_identity().revision > baseline.persistence_identity().revision
                || current.last_activity() > baseline.last_activity()
                || current.persistence_identity().sessions
                    != baseline.persistence_identity().sessions
                || current.persistence_identity().retired_sessions
                    != baseline.persistence_identity().retired_sessions);
        let (mut published, removals) = if merged_local {
            merge_local(
                &current,
                &baseline,
                &committed,
                rebound
                    .source_sessions
                    .as_ref()
                    .context("Replacement source sessions missing")?,
                &rebound.retired_local_sessions,
            )
        } else {
            (committed, Vec::new())
        };
        let basic = published
            .container_info()
            .context("Committed Agent container missing")?;
        let identity = published
            .persistence_identity()
            .container
            .clone()
            .context("Committed Agent identity missing")?;
        if self
            .inner
            .publish_verified_agent_registration(expected, published.clone())?
        {
            let mut containers = self
                .container_registration
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if containers.get(&basic.container_name).is_none_or(|current| {
                current.generation != identity.generation || current.revision <= identity.revision
            }) {
                containers.insert(basic.container_name, identity);
            }
            drop(containers);
            if merged_local {
                for (session_id, generation) in removals {
                    self.enqueue_structural(super::persist_ops::PersistOp::RemoveSession {
                        session_id,
                        generation,
                    });
                }
                published = (*self.prepare_info(Arc::new(published), &registration)?).clone();
                self.inner.insert(
                    published.project_id().to_owned(),
                    Arc::new(published.clone()),
                )?;
                let mut op = super::persist_ops::registration_for_info(&published)?;
                if let super::persist_ops::PersistOp::RegisterProject { project, .. } = &mut op {
                    project.service_type = Some(rebound.project_service_type);
                }
                self.enqueue_structural(op);
            }
        }
        Ok(PersistenceWriteOutcome::Committed)
    }
}

fn merge_local(
    current: &ProjectAndContainerInfo,
    baseline: &ProjectAndContainerInfo,
    committed: &ProjectAndContainerInfo,
    source_sessions: &BTreeMap<String, String>,
    retired_local: &BTreeMap<String, String>,
) -> (ProjectAndContainerInfo, Vec<(String, String)>) {
    // The SQL snapshot wins over an old cache. Only structural intent accepted
    // after admission can replace its newer request/model metadata.
    let mut merged =
        if current.persistence_identity().revision > baseline.persistence_identity().revision {
            current.clone()
        } else {
            committed.clone()
        };
    let mut identity = committed.persistence_identity().clone();
    identity
        .retired_sessions
        .extend(current.persistence_identity().retired_sessions.clone());
    let mut removals = Vec::new();
    for (sid, source_generation) in source_sessions {
        if current.persistence_identity().retired_sessions.get(sid) == Some(source_generation)
            && current.persistence_identity().sessions.get(sid) != Some(source_generation)
            && let Some(migrated_generation) = identity.sessions.remove(sid)
        {
            identity
                .retired_sessions
                .insert(sid.clone(), migrated_generation.clone());
            removals.push((sid.clone(), migrated_generation));
        }
    }
    for sid in current.sessions() {
        let generation = current.persistence_identity().sessions.get(&sid);
        if retired_local
            .get(&sid)
            .is_some_and(|retired| Some(retired) == generation)
        {
            continue;
        }
        identity
            .sessions
            .entry(sid)
            .or_insert_with(|| uuid::Uuid::new_v4().to_string());
    }
    let latest = merged.latest_session().map(str::to_owned);
    merged.clear_all_sessions();
    for (sid, generation) in &identity.sessions {
        merged.restore_session_identity(sid, generation.clone());
    }
    merged.restore_latest_session(
        latest
            .as_deref()
            .filter(|sid| identity.sessions.contains_key(*sid)),
    );
    merged.set_persistence_identity(identity);
    merged.set_container(committed.container_info());
    merged.set_timestamps(
        committed.created_at(),
        current.last_activity().max(committed.last_activity()),
    );
    (merged, removals)
}

async fn merge_admitted(
    tx: &mut dyn Executor,
    persisted: ProjectAndContainerInfo,
    admitted: &ProjectAndContainerInfo,
) -> Result<(ProjectAndContainerInfo, BTreeMap<String, String>)> {
    if persisted.persistence_identity().generation != admitted.persistence_identity().generation {
        return Ok((persisted, BTreeMap::new()));
    }
    let mut sessions = persisted.persistence_identity().sessions.clone();
    for (sid, generation) in &admitted.persistence_identity().retired_sessions {
        if sessions.get(sid) == Some(generation)
            && admitted.persistence_identity().sessions.get(sid) != Some(generation)
        {
            sessions.remove(sid);
        }
    }
    let mut retired = BTreeMap::new();
    for sid in admitted.sessions() {
        if sessions.contains_key(&sid) {
            continue;
        }
        let generation = admitted
            .persistence_identity()
            .sessions
            .get(&sid)
            .context("Admitted session identity missing")?;
        let tombstone = toasty::sql::query(
            "SELECT session_id FROM session_tombstones WHERE session_id=$1 AND generation=$2",
        )
        .bind(&sid)
        .bind(generation)
        .exec(tx)
        .await?;
        if tombstone.is_empty() {
            sessions.insert(sid, generation.clone());
        } else {
            retired.insert(sid, generation.clone());
        }
    }
    let mut merged =
        if admitted.persistence_identity().revision > persisted.persistence_identity().revision {
            admitted.clone()
        } else {
            persisted.clone()
        };
    let latest = merged.latest_session().map(str::to_owned);
    merged.clear_all_sessions();
    for (sid, generation) in &sessions {
        merged.restore_session_identity(sid, generation.clone());
    }
    merged.restore_latest_session(latest.as_deref().filter(|sid| sessions.contains_key(*sid)));
    let mut identity = persisted.persistence_identity().clone();
    identity.sessions = sessions;
    identity
        .retired_sessions
        .extend(admitted.persistence_identity().retired_sessions.clone());
    merged.set_persistence_identity(identity);
    merged.set_timestamps(
        persisted.created_at(),
        persisted.last_activity().max(admitted.last_activity()),
    );
    Ok((merged, retired))
}

async fn replace_rows(
    tx: &mut dyn Executor,
    expected: &ProjectAndContainerInfo,
    admitted: &ProjectAndContainerInfo,
    evidence: &VerifiedAgentReplacement,
) -> Result<Option<ReboundRegistration>> {
    crate::adapter::validate_agent_replacement(expected, evidence)?;
    let expected_container = expected.persistence_identity().container.as_ref();
    let service_type = evidence.service_type.family_representative();
    let mut keys = BTreeSet::from([
        format!("container-name:{}", evidence.replacement.container_name),
        format!("container:{}", evidence.replacement.container_id),
        format!("project:{}", expected.project_id()),
    ]);
    if let Some(previous) = &evidence.previous {
        keys.insert(format!("container:{}", previous.container_id));
    }
    keys.extend(
        expected
            .sessions()
            .into_iter()
            .map(|sid| format!("session:{sid}")),
    );
    for key in keys {
        toasty::sql::query("SELECT 1 FROM pg_advisory_xact_lock(hashtextextended($1, 719324))")
            .bind(key)
            .exec(tx)
            .await?;
    }
    let Some(project) = models::Project::filter_by_project_id(expected.project_id())
        .first()
        .exec(tx)
        .await?
    else {
        return Ok(None);
    };
    let project_service_type = project
        .service_type
        .clone()
        .context("Agent project service type missing")?;
    let project_family = project_service_type
        .parse::<shared_types::ServiceType>()
        .map_err(anyhow::Error::msg)?;
    if project.user_id.as_deref() != expected.user_id()
        || project.pod_id.as_deref() != expected.pod_id()
        || project.tenant_id.as_deref() != expected.tenant_id()
        || project.space_id.as_deref() != expected.space_id()
        || project.isolation_type.as_deref() != expected.isolation_type()
        || !project_family.same_container_family(service_type)
    {
        return Ok(None);
    }
    let old_project = project.generation == expected.persistence_identity().generation;
    let mut container =
        models::Container::filter_by_container_name(&evidence.replacement.container_name)
            .first()
            .exec(tx)
            .await?;
    if container.is_none() {
        // A fresh owned runtime may repair an existing detached project. No
        // predecessor is invented and no existing physical identity is retired.
        if !old_project || evidence.previous.is_some() || project.container_name.is_some() {
            return Ok(None);
        }
        let identity = ContainerPersistenceIdentity {
            generation: uuid::Uuid::new_v4().to_string(),
            revision: 1,
            physical_uid: Some(evidence.replacement.container_id.clone()),
            predecessor: None,
            predecessor_revision: None,
        };
        let snapshot = ContainerSnapshot::from_info(
            &evidence.replacement.container_name,
            &evidence.replacement,
            &service_type,
            &identity,
            evidence.replacement.workload_uid.as_deref(),
        )?;
        ensure!(
            repo::upsert_verified_replacement_container(tx, &snapshot).await?
                == PersistenceOperationOutcome::Committed,
            "Fresh Agent registration conflicts with a retired identity"
        );
        container =
            models::Container::filter_by_container_name(&evidence.replacement.container_name)
                .first()
                .exec(tx)
                .await?;
    }
    let container = container.context("Agent container registration disappeared")?;
    ensure!(
        container.logical_id == evidence.identifier
            && container
                .service_type
                .parse::<shared_types::ServiceType>()
                .map_err(anyhow::Error::msg)?
                .same_container_family(service_type),
        "Agent replacement container belongs to another owner"
    );
    let already_replaced = container.container_id.as_deref()
        == Some(evidence.replacement.container_id.as_str())
        && container.workload_uid == evidence.replacement.workload_uid;
    let binding_is_current = project.container_name.as_deref()
        == Some(container.container_name.as_str())
        && project.container_generation.as_deref() == Some(container.container_generation.as_str());
    if !old_project {
        let retired = toasty::sql::query(
            "SELECT project_id FROM project_tombstones WHERE project_id=$1 AND generation=$2",
        )
        .bind(expected.project_id())
        .bind(&expected.persistence_identity().generation)
        .exec(tx)
        .await?;
        if !already_replaced || retired.is_empty() || !binding_is_current {
            return Ok(None);
        }
    } else if let Some(previous) = &evidence.previous {
        let expected_container =
            expected_container.context("Expected Agent registration identity missing")?;
        if !already_replaced {
            if container.container_generation != expected_container.generation
                || container.container_id.as_deref() != Some(previous.container_id.as_str())
                || container.workload_uid != previous.workload_uid
                || !binding_is_current
            {
                return Ok(None);
            }
        } else {
            let retired = toasty::sql::query("SELECT container_name FROM container_tombstones WHERE container_name=$1 AND container_generation=$2 AND physical_uid=$3")
                .bind(&container.container_name).bind(&expected_container.generation).bind(&previous.container_id).exec(tx).await?;
            if retired.is_empty() || (project.container_name.is_some() && !binding_is_current) {
                return Ok(None);
            }
        }
    } else if !already_replaced || (project.container_name.is_some() && !binding_is_current) {
        // None proves no old-runtime absence. It may bind only the already
        // verified physical registration, never replace another container row.
        return Ok(None);
    }
    let fields = models::Session::fields();
    let sessions = models::Session::filter(
        fields
            .project_id()
            .eq(&project.project_id)
            .and(fields.project_generation().eq(&project.generation)),
    )
    .exec(tx)
    .await?
    .into_iter()
    .map(repo::SessionRow::try_from)
    .collect::<Result<Vec<_>>>()?;
    let row = repo::ProjectRow::from_model(project, sessions)?;
    let container = repo::ContainerRow::try_from(container)?;
    let current_identity = container.persistence_identity();
    let container_map = if row.container_name.is_some() {
        HashMap::from([(container.container_name.clone(), container)])
    } else {
        HashMap::new()
    };
    let mut old = load::hydrate_project(&row, &container_map)?;
    if !old_project {
        return Ok(Some(ReboundRegistration {
            project: old,
            project_service_type,
            source_sessions: None,
            retired_local_sessions: BTreeMap::new(),
        }));
    }
    let (merged, retired_local_sessions) = merge_admitted(tx, old, admitted).await?;
    old = merged;
    let source_sessions = old.persistence_identity().sessions.clone();
    let identity = if already_replaced {
        current_identity
    } else {
        let current = container_map
            .get(&evidence.replacement.container_name)
            .context("Old Agent container missing")?;
        let identity = ContainerPersistenceIdentity {
            generation: uuid::Uuid::new_v4().to_string(),
            revision: 1,
            physical_uid: Some(evidence.replacement.container_id.clone()),
            predecessor: Some(current.container_generation.clone()),
            predecessor_revision: Some(current.row_revision),
        };
        let snapshot = ContainerSnapshot::from_info(
            &evidence.replacement.container_name,
            &evidence.replacement,
            &service_type,
            &identity,
            evidence.replacement.workload_uid.as_deref(),
        )?;
        ensure!(
            repo::upsert_verified_replacement_container(tx, &snapshot).await?
                == PersistenceOperationOutcome::Committed,
            "Verified Agent container replacement lost its captured predecessor"
        );
        identity
    };
    let mut project_identity = ProjectPersistenceIdentity {
        revision: 1,
        predecessor: Some(old.persistence_identity().generation.clone()),
        container: Some(identity),
        sessions: BTreeMap::new(),
        ..Default::default()
    };
    for sid in old.sessions() {
        project_identity
            .sessions
            .insert(sid, uuid::Uuid::new_v4().to_string());
    }
    old.set_persistence_identity(project_identity);
    old.set_container(Some(evidence.replacement.clone()));
    let mut project_snapshot = ProjectSnapshot::from_info(&old)?;
    project_snapshot.service_type = Some(project_service_type.clone());
    ensure!(
        repo::upsert_project(tx, &project_snapshot).await?
            == PersistenceOperationOutcome::Committed,
        "Verified Agent project replacement lost its captured predecessor"
    );
    Ok(Some(ReboundRegistration {
        project: old,
        project_service_type,
        source_sessions: Some(source_sessions),
        retired_local_sessions,
    }))
}
