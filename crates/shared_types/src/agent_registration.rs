//! In-memory runtime evidence for registration-only Agent workload recovery.
//! These snapshots never authorize resource mutation or replace storage CAS.

use crate::{ContainerBasicInfo, ServiceType};

/// Registration changes are physical observations, not resource mutation authority.
/// A controller can retain its UID while Kubernetes replaces its Pod.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentRegistrationChange {
    Attached,
    PodRecreated,
    WorkloadReplaced,
}

impl AgentRegistrationChange {
    /// Classify captured physical identities. Ownership, predecessor absence and
    /// workspace continuity still require a fresh runtime verification.
    pub fn between(
        previous: Option<&ContainerBasicInfo>,
        replacement: &ContainerBasicInfo,
    ) -> Option<Self> {
        let workload = replacement
            .workload_uid
            .as_deref()
            .filter(|uid| !uid.is_empty())?;
        if replacement.container_id.is_empty() {
            return None;
        }
        let Some(previous) = previous else {
            return Some(Self::Attached);
        };
        let previous_workload = previous
            .workload_uid
            .as_deref()
            .filter(|uid| !uid.is_empty())?;
        if previous.container_id.is_empty() || previous.container_id == replacement.container_id {
            return None;
        }
        Some(if previous_workload == workload {
            Self::PodRecreated
        } else {
            Self::WorkloadReplaced
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentWorkspaceIdentity {
    pub namespace: String,
    pub claim_name: String,
    pub claim_uid: String,
    pub mount_path: String,
    pub sub_path: Option<String>,
}

/// The runtime verifies current ownership and workspace, and, when provided,
/// confirms the captured predecessor Pod is absent. A retained controller can
/// legitimately own a new Pod; a replaced controller must itself be absent.
/// A detached project has no old
/// physical identity to assert; attaching it authorizes registration only. Storage must
/// still compare the captured registration generation when committing it.
/// Deliberately has no persisted or public HTTP representation.
#[derive(Clone, Debug)]
pub struct VerifiedAgentReplacement {
    pub identifier: String,
    pub service_type: ServiceType,
    pub previous: Option<ContainerBasicInfo>,
    pub replacement: ContainerBasicInfo,
    pub workspace: AgentWorkspaceIdentity,
}

impl VerifiedAgentReplacement {
    pub fn change(&self) -> Option<AgentRegistrationChange> {
        AgentRegistrationChange::between(self.previous.as_ref(), &self.replacement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(pod: &str, workload: Option<&str>) -> ContainerBasicInfo {
        ContainerBasicInfo {
            container_id: pod.into(),
            container_name: "owned-agent".into(),
            container_ip: "127.0.0.1".into(),
            internal_port: 8086,
            external_port: 0,
            project_id: "owner".into(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: "http://127.0.0.1:8086".into(),
            workload_uid: workload.map(str::to_owned),
        }
    }

    #[test]
    fn registration_change_distinguishes_attachment_pod_recreation_and_workload_replacement() {
        let old = basic("old-pod", Some("retained-controller"));
        let new = basic("new-pod", Some("retained-controller"));
        assert_eq!(
            AgentRegistrationChange::between(None, &new),
            Some(AgentRegistrationChange::Attached)
        );
        assert_eq!(
            AgentRegistrationChange::between(Some(&old), &new),
            Some(AgentRegistrationChange::PodRecreated)
        );
        assert_eq!(
            AgentRegistrationChange::between(
                Some(&old),
                &basic("other-pod", Some("other-controller"))
            ),
            Some(AgentRegistrationChange::WorkloadReplaced)
        );
    }

    #[test]
    fn registration_change_never_invents_unknown_or_unchanged_physical_identity() {
        let old = basic("old-pod", Some("retained-controller"));
        for new in [
            old.clone(),
            basic("old-pod", Some("other-controller")),
            basic("", Some("retained-controller")),
            basic("new-pod", None),
            basic("new-pod", Some("")),
        ] {
            assert_eq!(AgentRegistrationChange::between(Some(&old), &new), None);
        }
        for old in [
            basic("", Some("retained-controller")),
            basic("old-pod", None),
            basic("old-pod", Some("")),
        ] {
            assert_eq!(
                AgentRegistrationChange::between(
                    Some(&old),
                    &basic("new-pod", Some("retained-controller"))
                ),
                None
            );
        }
        assert_eq!(
            AgentRegistrationChange::between(None, &basic("docker-id", None)),
            None
        );
    }
}
