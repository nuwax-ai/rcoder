//! Shared validation and the memory backend of verified Agent re-registration.
use super::ProjectAdapter;
use anyhow::{Result, ensure};
use shared_types::{
    ProjectAndContainerInfo, ServiceType, VerifiedAgentReplacement,
    persistence::{ContainerPersistenceIdentity, ProjectPersistenceIdentity},
};

pub(crate) fn validate_agent_replacement(
    expected: &ProjectAndContainerInfo,
    evidence: &VerifiedAgentReplacement,
) -> Result<()> {
    ensure!(
        matches!(
            evidence.service_type.family_representative(),
            ServiceType::ComputerAgentRunner | ServiceType::WebAgentRunner
        ) && expected
            .service_type()
            .is_some_and(|service_type| service_type.same_container_family(evidence.service_type))
            && expected.container_key() == evidence.identifier
            && evidence.replacement.project_id == evidence.identifier
            && !evidence.replacement.container_id.is_empty()
            && !evidence.replacement.container_name.is_empty()
            && evidence
                .replacement
                .workload_uid
                .as_ref()
                .is_some_and(|uid| !uid.is_empty())
            && !evidence.workspace.namespace.is_empty()
            && !evidence.workspace.claim_name.is_empty()
            && !evidence.workspace.claim_uid.is_empty()
            && !evidence.workspace.mount_path.is_empty(),
        "Verified Agent replacement evidence differs from registry ownership"
    );
    match (expected.container_info(), evidence.previous.as_ref()) {
        (Some(previous), Some(verified)) => {
            ensure!(
                previous.container_id == verified.container_id
                    && previous.container_name == verified.container_name
                    && previous.workload_uid == verified.workload_uid
                    && verified.project_id == evidence.identifier
                    && verified.container_name == evidence.replacement.container_name
                    && !verified.container_id.is_empty()
                    && verified.container_id != evidence.replacement.container_id
                    && verified
                        .workload_uid
                        .as_ref()
                        .is_some_and(|uid| !uid.is_empty())
                    && verified.workload_uid != evidence.replacement.workload_uid
                    && expected
                        .persistence_identity()
                        .container
                        .as_ref()
                        .is_none_or(|identity| {
                            identity.revision > 0
                                && identity.physical_uid.as_deref()
                                    == Some(verified.container_id.as_str())
                        }),
                "Expected Agent container differs from verified predecessor"
            );
        }
        (None, None) => ensure!(
            expected.persistence_identity().container.is_none(),
            "Detached Agent registration retains a conflicting physical identity"
        ),
        _ => anyhow::bail!("Agent predecessor presence differs from the captured registration"),
    }
    Ok(())
}

impl ProjectAdapter {
    pub(crate) fn replace_agent_container_registration(
        &self,
        expected: &ProjectAndContainerInfo,
        evidence: &VerifiedAgentReplacement,
    ) -> Result<bool> {
        validate_agent_replacement(expected, evidence)?;
        let Some(current) = self.get(expected.project_id()) else {
            return Ok(false);
        };
        if current.persistence_identity().generation != expected.persistence_identity().generation {
            return Ok(current.container_info().is_some_and(|container| {
                container.container_id == evidence.replacement.container_id
                    && container.workload_uid == evidence.replacement.workload_uid
            }));
        }
        let generation = self
            .iter()
            .into_iter()
            .find_map(|(_, project)| {
                project
                    .container_info()
                    .filter(|basic| {
                        basic.container_id == evidence.replacement.container_id
                            && basic.workload_uid == evidence.replacement.workload_uid
                    })
                    .and_then(|_| project.persistence_identity().container.clone())
            })
            .unwrap_or_else(|| ContainerPersistenceIdentity {
                generation: uuid::Uuid::new_v4().to_string(),
                revision: 1,
                physical_uid: Some(evidence.replacement.container_id.clone()),
                predecessor: expected
                    .persistence_identity()
                    .container
                    .as_ref()
                    .map(|c| c.generation.clone()),
                predecessor_revision: expected
                    .persistence_identity()
                    .container
                    .as_ref()
                    .map(|c| c.revision),
            });
        let mut identity = ProjectPersistenceIdentity {
            revision: 1,
            predecessor: Some(current.persistence_identity().generation.clone()),
            container: Some(generation),
            ..Default::default()
        };
        for sid in current.sessions() {
            identity
                .sessions
                .insert(sid.clone(), uuid::Uuid::new_v4().to_string());
            if let Some(previous) = current.persistence_identity().sessions.get(&sid) {
                identity.retired_sessions.insert(sid, previous.clone());
            }
        }
        let mut replacement = (*current).clone();
        replacement.set_persistence_identity(identity);
        replacement.set_container(Some(evidence.replacement.clone()));
        self.publish_verified_agent_registration(expected, replacement)
    }
}
