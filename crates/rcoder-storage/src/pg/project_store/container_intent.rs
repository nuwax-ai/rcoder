//! A known rejected registration must not leave its optimistic container cursor
//! ahead of SQL. Only that exact still-current intent may be corrected.
use crate::{adapter::container_entry_key, pg::PgStore};
use shared_types::{
    ContainerBasicInfo, ProjectAndContainerInfo, ServiceType,
    persistence::ContainerPersistenceIdentity,
};
use std::sync::Arc;
use tokio::time::Instant;

#[derive(Clone)]
pub(crate) struct RejectedContainerIntent {
    identity: ContainerPersistenceIdentity,
    physical: ContainerBasicInfo,
    service_type: ServiceType,
    project: Option<Arc<ProjectAndContainerInfo>>,
}

pub(super) struct ReconciledContainer {
    rejected: ContainerPersistenceIdentity,
    canonical: ContainerPersistenceIdentity,
}

impl ReconciledContainer {
    pub(super) fn rebase_fresh_info(&self, info: &mut Arc<ProjectAndContainerInfo>) {
        if info.persistence_identity().container.as_ref() == Some(&self.rejected) {
            let mut identity = info.persistence_identity().clone();
            identity.container = Some(self.canonical.clone());
            Arc::make_mut(info).set_persistence_identity(identity);
        }
    }
}

impl PgStore {
    fn discard_obsolete_container_rejection(
        &self,
        name: &str,
        rejected: &Arc<RejectedContainerIntent>,
    ) {
        let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
        let mut rejected_intents = self
            .rejected_container_intents
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if rejected_intents
            .get(name)
            .is_some_and(|current| Arc::ptr_eq(current, rejected))
        {
            rejected_intents.remove(name);
        }
    }
    pub(super) fn record_rejected_container_intent(&self, captured: &Arc<ProjectAndContainerInfo>) {
        let (Some(identity), Some(physical), Some(service_type)) = (
            captured.persistence_identity().container.as_ref(),
            captured.container_info(),
            captured.service_type(),
        ) else {
            return;
        };
        let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
        let name = container_entry_key(captured);
        if self
            .container_registration
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&name)
            != Some(identity)
        {
            return;
        }
        let project = self
            .inner
            .get(captured.project_id())
            .filter(|current| Arc::ptr_eq(current, captured));
        self.rejected_container_intents
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                name,
                Arc::new(RejectedContainerIntent {
                    identity: identity.clone(),
                    physical,
                    service_type,
                    project,
                }),
            );
    }

    pub(super) async fn reconcile_rejected_container_intent(
        &self,
        name: &str,
        deadline: Instant,
    ) -> anyhow::Result<Option<ReconciledContainer>> {
        let rejected = self
            .rejected_container_intents
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(name)
            .cloned();
        let Some(rejected) = rejected else {
            return Ok(None);
        };
        {
            let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            if self
                .container_registration
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(name)
                != Some(&rejected.identity)
            {
                // A newer intent owns the cursor; never lower it to recover an
                // older known failure. Its own completion decides that intent.
                let mut rejected_intents = self
                    .rejected_container_intents
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if rejected_intents
                    .get(name)
                    .is_some_and(|current| Arc::ptr_eq(current, &rejected))
                {
                    rejected_intents.remove(name);
                }
                return Ok(None);
            }
        }
        let key = name.to_owned();
        let checked = Arc::clone(&rejected);
        let (row, project) = tokio::time::timeout_at(
            deadline,
            super::database::read(&self.database, move |tx| {
                Box::pin(async move {
                    let container = super::repo::fetch_container_registration(tx, &key).await?;
                    let project = if let Some(previous) = checked.project.as_ref() {
                        if let Some((row, container)) =
                            super::repo::fetch_project_registration(tx, previous.project_id())
                                .await?
                        {
                            let containers = container
                                .map(|row| {
                                    std::collections::HashMap::from([(
                                        row.container_name.clone(),
                                        row,
                                    )])
                                })
                                .unwrap_or_default();
                            let persisted = super::load::hydrate_project(&row, &containers)?;
                            (previous.persistence_identity().generation == row.generation
                                && super::project_stop::same_stop_scope(previous, &persisted))
                            .then_some(persisted)
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    Ok((container, project))
                })
            }),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "Known rejected container cursor readback exceeded the durable deadline"
            )
        })??;
        let Some(row) = row else {
            self.discard_obsolete_container_rejection(name, &rejected);
            anyhow::bail!("Rejected container registration is missing from SQL");
        };
        let canonical = {
            if !(row.container_generation == rejected.identity.generation
                && row.container_id == rejected.identity.physical_uid
                && row.container_id.as_deref() == Some(rejected.physical.container_id.as_str())
                && row.workload_uid == rejected.physical.workload_uid
                && row.logical_id == rejected.physical.project_id
                && row
                    .service_type
                    .parse::<ServiceType>()
                    .map_err(anyhow::Error::msg)?
                    .same_container_family(rejected.service_type))
            {
                self.discard_obsolete_container_rejection(name, &rejected);
                anyhow::bail!(
                    "Rejected container cursor now belongs to a different runtime or owner"
                );
            }
            row.persistence_identity()
        };
        let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
        let mut rejected_intents = self
            .rejected_container_intents
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // Prevent an old readback from correcting a new intent that happens to
        // reuse the same generation/revision tuple after another correction.
        if !rejected_intents
            .get(name)
            .is_some_and(|current| Arc::ptr_eq(current, &rejected))
        {
            return Ok(None);
        }
        let mut containers = self
            .container_registration
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if containers.get(name) != Some(&rejected.identity) {
            return Ok(None);
        }
        containers.insert(name.to_owned(), canonical.clone());
        drop(containers);
        // Project metadata is untouched. Fresh callers already holding the old
        // cursor receive the same exact proof below; newer captures read this
        // corrected physical cursor from their current project projection.
        for (id, current) in self.inner.iter() {
            if current.persistence_identity().container.as_ref() == Some(&rejected.identity) {
                let mut info = (*current).clone();
                let mut identity = info.persistence_identity().clone();
                identity.container = Some(canonical.clone());
                if let Some(persisted) = project.as_ref()
                    && rejected
                        .project
                        .as_ref()
                        .is_some_and(|captured| Arc::ptr_eq(captured, &current))
                {
                    // Revert only this undispatched request's rejected fields.
                    // Accepted SID routing, retired markers, selection, status
                    // and activity from before its capture remain intact.
                    info.set_model_provider(persisted.model_provider().cloned());
                    info.set_request_id(persisted.request_id().map(str::to_owned));
                    info.set_service_type(persisted.service_type());
                    identity.revision = persisted.persistence_identity().revision;
                }
                info.set_persistence_identity(identity);
                self.inner.insert(id, Arc::new(info))?;
            }
        }
        rejected_intents.remove(name);
        Ok(Some(ReconciledContainer {
            rejected: rejected.identity.clone(),
            canonical,
        }))
    }
}
