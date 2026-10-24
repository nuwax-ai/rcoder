//! Preparation reservations are separate from real ACP session registrations.

use std::sync::Arc;

use dashmap::mapref::entry::Entry;
use shared_types::{AgentStatus, CancelNotificationRequestWrapper};
use tokio::sync::mpsc::Sender;

use super::AgentSessionRegistry;

struct PendingExecution {
    session_id: agent_client_protocol::schema::v1::SessionId,
    cancel_tx: Sender<CancelNotificationRequestWrapper>,
    request_id: Option<String>,
}

pub(super) struct PendingPreparation {
    incarnation: Arc<()>,
    guards: usize,
    manual: bool,
    preserve_execution: bool,
    rollback: Option<PendingExecution>,
}

enum PreparationLease {
    Guard,
    Manual,
}

/// RAII lease for request preparation. An unassigned session has no registry
/// entry; successful or uncertain dispatch leaves real execution state alone.
pub struct PendingGuard<'a> {
    registry: &'a AgentSessionRegistry,
    project_id: String,
    incarnation: Arc<()>,
    preserve_execution: bool,
}

impl<'a> PendingGuard<'a> {
    pub fn new(registry: &'a AgentSessionRegistry, project_id: &str) -> Self {
        Self {
            registry,
            project_id: project_id.into(),
            incarnation: registry.reserve_preparation(project_id, PreparationLease::Guard),
            preserve_execution: false,
        }
    }

    pub fn commit_success(mut self) {
        self.preserve_execution = true;
    }

    /// A timeout is not proof that an accepted prompt never started.
    pub(crate) fn retain_unknown_outcome(&mut self) {
        self.preserve_execution = true;
    }
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.registry.release_preparation(
            &self.project_id,
            Some(&self.incarnation),
            self.preserve_execution,
        );
    }
}

#[derive(Debug, Clone)]
pub struct RegistryStats {
    pub agent_count: usize,
    pub session_count: usize,
}

impl AgentSessionRegistry {
    /// Compatibility for callers managing a manual preparation reservation.
    /// This never constructs a SessionId or publishes a session mapping.
    pub fn set_pending(&self, project_id: &str) {
        self.reserve_preparation(project_id, PreparationLease::Manual);
    }

    pub fn pending_preparations(&self) -> usize {
        self.preparations.len()
    }

    pub fn is_pending(&self, project_id: &str) -> bool {
        self.preparations.contains_key(project_id)
    }

    pub fn clear_pending_if_exists(&self, project_id: &str) {
        self.release_preparation(project_id, None, false);
    }

    fn reserve_preparation(&self, project_id: &str, lease: PreparationLease) -> Arc<()> {
        // Lock order: preparation -> real agent. No guard crosses an await.
        let mut entry = self
            .preparations
            .entry(project_id.to_owned())
            .or_insert_with(|| {
                let rollback = self
                    .agent_info_map
                    .get_mut(project_id)
                    .and_then(|mut info| match info.status {
                        AgentStatus::Idle => {
                            let previous = PendingExecution {
                                session_id: info.session_id.clone(),
                                cancel_tx: info.cancel_tx.clone(),
                                request_id: info.request_id.clone(),
                            };
                            info.status = AgentStatus::Pending;
                            info.last_activity = chrono::Utc::now();
                            Some(previous)
                        }
                        AgentStatus::Pending | AgentStatus::Active | AgentStatus::Terminating => {
                            None
                        }
                    });
                PendingPreparation {
                    incarnation: Arc::new(()),
                    guards: 0,
                    manual: false,
                    preserve_execution: false,
                    rollback,
                }
            });
        match lease {
            PreparationLease::Guard => entry.guards += 1,
            PreparationLease::Manual => entry.manual = true,
        }
        entry.incarnation.clone()
    }

    fn release_preparation(
        &self,
        project_id: &str,
        incarnation: Option<&Arc<()>>,
        preserve_execution: bool,
    ) {
        match self.preparations.entry(project_id.to_owned()) {
            Entry::Occupied(mut entry) => {
                match incarnation {
                    Some(incarnation) => {
                        if !Arc::ptr_eq(incarnation, &entry.get().incarnation)
                            || entry.get().guards == 0
                        {
                            return;
                        }
                        entry.get_mut().guards -= 1;
                    }
                    None => entry.get_mut().manual = false,
                }
                entry.get_mut().preserve_execution |= preserve_execution;
                if entry.get().guards != 0 || entry.get().manual {
                    return;
                }
                // Keep this incarnation locked until rollback completes. A new
                // reservation cannot inherit Pending and then be reset by us.
                if !entry.get().preserve_execution
                    && let Some(previous) = entry.get().rollback.as_ref()
                    && let Some(mut info) = self.agent_info_map.get_mut(project_id)
                    && info.status == AgentStatus::Pending
                    && info.session_id == previous.session_id
                    && info.cancel_tx.same_channel(&previous.cancel_tx)
                    && info.request_id == previous.request_id
                {
                    info.status = AgentStatus::Idle;
                }
                entry.remove();
            }
            Entry::Vacant(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_abstraction::traits::SessionRegistry;

    fn registry_with_idle() -> AgentSessionRegistry {
        let registry = AgentSessionRegistry::new();
        registry.register(
            "project",
            "opaque-session",
            super::super::tests::create_test_agent_info("project", "opaque-session"),
        );
        registry
    }

    #[test]
    fn preparation_reservation_never_exposes_a_session_identity() {
        let registry = AgentSessionRegistry::new();
        let guard = PendingGuard::new(&registry, "project");
        assert!(registry.is_pending("project"));
        assert!(registry.get_agent_info("project").is_none());
        assert!(registry.view_agent_info("project", Clone::clone).is_none());
        assert!(registry.get_session_by_project("project").is_none());
        assert!(!SessionRegistry::contains(&registry, "project"));
        assert_eq!(registry.stats().agent_count, 0);
        assert_eq!(registry.stats().session_count, 0);
        guard.commit_success();
        assert!(!registry.is_pending("project"));
        assert!(!registry.contains_project("project"));
    }

    #[test]
    fn overlapping_guards_release_only_their_own_reservation() {
        let registry = registry_with_idle();
        let first = PendingGuard::new(&registry, "project");
        let second = PendingGuard::new(&registry, "project");
        assert_eq!(
            registry.view_agent_info("project", |info| info.status),
            Some(AgentStatus::Pending)
        );
        drop(first);
        assert!(registry.is_pending("project"));
        assert_eq!(
            registry.view_agent_info("project", |info| info.status),
            Some(AgentStatus::Pending)
        );
        drop(second);
        assert!(!registry.is_pending("project"));
        assert_eq!(
            registry.view_agent_info("project", |info| info.status),
            Some(AgentStatus::Idle)
        );
        assert_eq!(
            registry.get_session_by_project("project").as_deref(),
            Some("opaque-session")
        );
    }

    #[test]
    fn accepted_dispatch_cannot_be_rolled_back_by_another_guard() {
        let registry = registry_with_idle();
        let first = PendingGuard::new(&registry, "project");
        let second = PendingGuard::new(&registry, "project");
        first.commit_success();
        drop(second);
        assert!(!registry.is_pending("project"));
        assert_eq!(
            registry.view_agent_info("project", |info| info.status),
            Some(AgentStatus::Pending)
        );
    }

    #[test]
    fn unknown_dispatch_outcome_preserves_real_execution_and_releases_reservation() {
        let registry = registry_with_idle();
        let mut guard = PendingGuard::new(&registry, "project");
        guard.retain_unknown_outcome();
        drop(guard);
        assert!(!registry.is_pending("project"));
        assert_eq!(
            registry.view_agent_info("project", |info| info.status),
            Some(AgentStatus::Pending)
        );
    }

    #[test]
    fn old_guard_cannot_reset_a_replacement_execution_with_the_same_session() {
        let registry = registry_with_idle();
        let guard = PendingGuard::new(&registry, "project");
        let mut replacement =
            super::super::tests::create_test_agent_info("project", "opaque-session");
        replacement.status = AgentStatus::Pending;
        registry.register("project", "opaque-session", replacement);
        drop(guard);
        assert_eq!(
            registry.view_agent_info("project", |info| info.status),
            Some(AgentStatus::Pending)
        );
    }

    #[test]
    fn removed_preparation_cannot_release_a_new_incarnation() {
        let registry = AgentSessionRegistry::new();
        let old = PendingGuard::new(&registry, "project");
        registry.remove_by_project("project");
        let current = PendingGuard::new(&registry, "project");
        drop(old);
        assert!(registry.is_pending("project"));
        drop(current);
        assert!(!registry.is_pending("project"));
    }

    #[test]
    fn manual_preparation_and_guard_have_independent_lifetimes() {
        let registry = AgentSessionRegistry::new();
        registry.set_pending("project");
        registry.set_pending("project");
        let guard = PendingGuard::new(&registry, "project");
        registry.clear_pending_if_exists("project");
        assert!(registry.is_pending("project"));
        drop(guard);
        assert!(!registry.is_pending("project"));
    }

    #[test]
    fn legacy_placeholder_is_not_a_session_but_real_pending_status_is() {
        let registry = AgentSessionRegistry::new();
        registry.register(
            "legacy",
            "pending",
            super::super::tests::create_test_agent_info("legacy", "pending"),
        );
        assert!(registry.get_agent_info("legacy").is_some());
        assert!(registry.view_agent_info("legacy", Clone::clone).is_some());
        assert!(registry.get_session_by_project("legacy").is_none());
        assert!(registry.get_agent_info_by_session("pending").is_none());
        assert!(
            registry
                .view_agent_info_by_session("pending", Clone::clone)
                .is_none()
        );
        assert!(registry.get_project_by_session("pending").is_none());
        assert!(SessionRegistry::get(&registry, "legacy").is_some());
        assert!(SessionRegistry::contains(&registry, "legacy"));
        assert!(!registry.contains_session("pending"));
        assert_eq!(registry.all_project_ids(), vec!["legacy"]);
        // Historical raw record is retained, not deleted as a recovery shortcut.
        assert!(registry.inner_mut().contains_key("legacy"));
        for sid in ["opaque-not-uuid", "pending-session", "Pending"] {
            let mut info = super::super::tests::create_test_agent_info("real", sid);
            info.status = AgentStatus::Pending;
            registry.register("real", sid, info);
            assert_eq!(
                registry
                    .view_agent_info("real", |info| info.session_id.to_string())
                    .as_deref(),
                Some(sid)
            );
            assert_eq!(
                registry.get_project_by_session(sid).as_deref(),
                Some("real")
            );
        }
    }
}
