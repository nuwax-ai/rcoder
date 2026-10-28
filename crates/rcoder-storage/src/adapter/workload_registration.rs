use super::ProjectAdapter;
use crate::workload_admission::{ContainerRegistration, ProjectRegistration, SessionRegistration};
use crate::{WorkloadKey, WorkloadRegistrationFrame};
use anyhow::{Result, ensure};
use shared_types::persistence::PersistenceWriteOutcome;
use std::sync::Arc;
#[cfg(feature = "pg")]
mod pg_mirror;

impl ProjectAdapter {
    pub(crate) fn workload_has_references(&self, key: &WorkloadKey) -> bool {
        self.containers.contains_key(key.logical_name())
            || self.iter().iter().any(|(_, project)| {
                project
                    .container_info()
                    .is_some_and(|physical| physical.container_name == key.logical_name())
            })
    }
    pub(crate) fn capture_workload_frame(
        &self,
        key: &WorkloadKey,
        owner: Arc<()>,
        name: &str,
    ) -> Result<WorkloadRegistrationFrame> {
        let container = self
            .containers
            .view(name, |_, entry| (entry.info(), entry.service_type()));
        if let Some((_, family)) = &container {
            ensure!(
                family.same_container_family(key.family()),
                "Canonical container belongs to another family"
            );
        }
        let mut memory = self
            .iter()
            .into_iter()
            .filter_map(|(_, project)| {
                project
                    .container_info()
                    .is_some_and(|physical| physical.container_name == name)
                    .then_some(project)
            })
            .collect::<Vec<_>>();
        memory.sort_by(|a, b| a.project_id().cmp(b.project_id()));
        let projects = memory
            .iter()
            .map(|project| {
                let identity = project.persistence_identity();
                ProjectRegistration {
                    id: project.project_id().into(),
                    user_id: project.user_id().map(str::to_owned),
                    generation: identity.generation.clone(),
                    revision: identity.revision,
                    container_generation: identity
                        .container
                        .as_ref()
                        .map(|container| container.generation.clone()),
                }
            })
            .collect();
        let mut sessions = Vec::new();
        for project in &memory {
            let identity = project.persistence_identity();
            for sid in project.sessions() {
                sessions.push(SessionRegistration {
                    generation: identity.sessions.get(&sid).cloned().unwrap_or_default(),
                    id: sid,
                    project_id: project.project_id().into(),
                    project_generation: identity.generation.clone(),
                });
            }
        }
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        let container = container.map(|(physical, family)| {
            let identity = memory
                .iter()
                .find_map(|project| project.persistence_identity().container.clone());
            ContainerRegistration {
                family: family.to_string(),
                generation: identity
                    .as_ref()
                    .map(|identity| identity.generation.clone())
                    .unwrap_or_default(),
                revision: identity
                    .map(|identity| identity.revision)
                    .unwrap_or_default(),
                physical_uid: (!physical.container_id.is_empty()).then_some(physical.container_id),
                workload_uid: physical.workload_uid,
            }
        });
        Ok(WorkloadRegistrationFrame {
            key: key.clone(),
            owner,
            container_name: name.into(),
            container,
            projects,
            sessions,
            memory,
        })
    }
    pub(crate) fn retire_workload_frame(
        &self,
        frame: &WorkloadRegistrationFrame,
    ) -> Result<PersistenceWriteOutcome> {
        // Match the existing project -> container lock order. The exclusive
        // admission guard covers supported callers; these locks also fence
        // registration writers which have not yet adopted that outer guard.
        let _projects = frame
            .projects
            .iter()
            .map(|project| self.project_locks.entry_by_ref(&project.id))
            .collect::<Vec<_>>();
        let _container = self.container_locks.entry_by_ref(&frame.container_name);
        let current =
            self.capture_workload_frame(&frame.key, frame.owner.clone(), &frame.container_name)?;
        if current.container != frame.container
            || current.projects != frame.projects
            || current.sessions != frame.sessions
            || current
                .memory
                .iter()
                .zip(&frame.memory)
                .any(|(current, old)| !Arc::ptr_eq(current, old))
        {
            return Ok(PersistenceWriteOutcome::Superseded);
        }
        for project in &frame.projects {
            self.remove_locked(&project.id);
        }
        if let Some((_, retired)) = self.containers.remove(&frame.container_name) {
            self.container_id_to_key
                .remove(&retired.info().container_id);
        }
        Ok(PersistenceWriteOutcome::Committed)
    }
}
