//! Apply a confirmed canonical retirement to only matching local projections.
use super::*;
use dashmap::mapref::entry::Entry;

impl ProjectAdapter {
    pub(crate) fn prune_committed_workload_frame(
        &self,
        frame: &WorkloadRegistrationFrame,
        container_cursor_matches: bool,
    ) -> bool {
        let _projects = frame
            .projects
            .iter()
            .map(|project| self.project_locks.entry_by_ref(&project.id))
            .collect::<Vec<_>>();
        let _container = self.container_locks.entry_by_ref(&frame.container_name);
        for expected in &frame.projects {
            let Some(current) = self.get(&expected.id) else {
                continue;
            };
            let identity = current.persistence_identity();
            if identity.generation != expected.generation
                || identity.revision != expected.revision
                || current.user_id() != expected.user_id.as_deref()
            {
                continue;
            }
            if let Some(container) = current.container_info()
                && (container.container_name != frame.container_name
                    || frame.physical_uid()
                        != (!container.container_id.is_empty())
                            .then_some(container.container_id.as_str())
                    || frame.workload_uid() != container.workload_uid.as_deref())
            {
                continue;
            }
            let sessions = current.sessions();
            if sessions.iter().any(|sid| {
                !frame.sessions.iter().any(|captured| {
                    captured.id == *sid
                        && captured.project_id == expected.id
                        && captured.project_generation == expected.generation
                        && identity
                            .sessions
                            .get(sid)
                            .is_none_or(|generation| *generation == captured.generation)
                })
            }) {
                continue;
            }
            // These project locks exclude same-project replacement. Other
            // projects can reuse a SID, so their newer reverse index is CASed.
            self.projects.remove(&expected.id);
            for sid in &sessions {
                self.session_index
                    .remove_if(sid, |_, (_, project)| project == &expected.id);
            }
            if let Some(user) = current.user_id()
                && let Entry::Occupied(entry) = self.user_id_to_project_ids.entry(user.into())
            {
                entry.get().remove(&expected.id);
                if entry.get().is_empty() {
                    entry.remove_entry();
                }
            }
            if let Some(pod) = current.pod_id() {
                self.pod_id_to_project_id
                    .remove_if(pod, |_, project| project == &expected.id);
            }
            if let Some((_, key)) = self.project_to_container.remove(&expected.id) {
                self.dec_container_ref(&key);
            }
        }
        if !container_cursor_matches
            || self.iter().iter().any(|(_, project)| {
                project
                    .container_info()
                    .is_some_and(|container| container.container_name == frame.container_name)
            })
        {
            return false;
        }
        let matched = self
            .containers
            .view(&frame.container_name, |_, entry| {
                let current = entry.info();
                frame.physical_uid()
                    == (!current.container_id.is_empty()).then_some(current.container_id.as_str())
                    && frame.workload_uid() == current.workload_uid.as_deref()
            })
            .unwrap_or(true);
        if !matched {
            return false;
        }
        if let Some((_, retired)) = self.containers.remove(&frame.container_name) {
            self.container_id_to_key
                .remove_if(&retired.info().container_id, |_, name| {
                    name == &frame.container_name
                });
        }
        true
    }
}
