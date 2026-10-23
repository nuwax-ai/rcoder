//! Computer registration recovery shared by chat and ordinary pod ensure.
//! Runtime observations authorize a replacement; the store owns its atomic
//! generation transition. No resource creation or deletion happens here.

use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use container_runtime_api::{ContainerCreateParams, ContainerRuntimeError};
use shared_types::{
    AgentRegistrationChange, AppError, ComputerOwnerMismatch, ContainerBasicInfo,
    ContainerWorkloadReplaced, ErrorDetail, ProjectAndContainerInfo, ProjectStore as _,
    persistence::PersistenceWriteOutcome,
};

use super::AppState;

/// Distinguish compute-only ensure from an actual chat metadata request.
/// None remains an explicit chat value; it never doubles as a preserve flag.
#[derive(Clone, Copy, Debug)]
pub enum RegistrationMetadata {
    PreserveChatMetadata,
    UpdateChatMetadata,
}

impl AppState {
    pub async fn ensure_computer_project_mapping(
        &self,
        params: &ContainerCreateParams,
        incoming: Arc<ProjectAndContainerInfo>,
        metadata: RegistrationMetadata,
    ) -> Result<Arc<ProjectAndContainerInfo>> {
        let project_id = incoming.project_id();
        let mut observed = incoming
            .container_info()
            .context("Computer registration has no observed container")?;

        let initial = self.projects.get(project_id);
        if let Some(previous) = initial.as_ref() {
            ensure_request_owner(previous, &incoming)?;
        }
        let (current_registration, refreshed) = self
            .refresh_inconsistent_computer_registration(project_id)
            .await?;
        let mut retired_request_registration =
            refreshed && current_registration.is_none() && self.projects.get(project_id).is_none();
        if refreshed && current_registration.is_none() && self.projects.get(project_id).is_some() {
            return Err(
                AppError::conflict("Computer registration was retired during refresh").into(),
            );
        }
        if let Some(current) = current_registration.as_ref() {
            ensure_request_owner(current, &incoming)?;
        }

        // A first request for project B may be the first observer of a shared
        // workload replacement. Recover its existing canonical registration A
        // before registering B; absence of B does not authorize overwriting A.
        let related = || match incoming.pod_id() {
            Some(pod_id) => self.projects.find_projects_by_pod_id(pod_id),
            None => incoming
                .user_id()
                .map(|user_id| {
                    self.projects
                        .find_projects_by_user_id(user_id, &params.service_type)
                })
                .unwrap_or_default(),
        };
        // Older ordinary registrations could mutate a shared Basic while a
        // project's captured physical generation still named its predecessor.
        // Read that registration from authoritative storage; never manufacture
        // predecessor evidence by substituting an old UID into a newer Basic.
        if let Some(expected) = related().into_iter().find(|project| {
            project.project_id() != project_id
                && project.container_key() == observed.project_id
                && project
                    .service_type()
                    .is_some_and(|kind| kind.same_container_family(params.service_type))
                && project.container_info().is_some_and(|basic| {
                    basic.container_name == observed.container_name
                        && (inconsistent_physical_registration(project)
                            || (AgentRegistrationChange::between(Some(&basic), &observed)
                                .is_some()
                                && project
                                    .persistence_identity()
                                    .container
                                    .as_ref()
                                    .is_none_or(|identity| {
                                        identity.physical_uid.as_deref()
                                            == Some(basic.container_id.as_str())
                                    })))
                })
        }) {
            let expected_id = expected.project_id().to_owned();
            let expected = if inconsistent_physical_registration(&expected) {
                let refreshed = self
                    .refresh_inconsistent_computer_registration(&expected_id)
                    .await?
                    .0;
                if refreshed.is_none() && self.projects.get(&expected_id).is_some() {
                    return Err(AppError::conflict(
                        "Canonical Computer registration was retired during refresh",
                    )
                    .into());
                }
                refreshed
            } else {
                Some(expected)
            };
            if let Some(expected) = expected
                && AgentRegistrationChange::between(expected.container_info().as_ref(), &observed)
                    .is_some()
            {
                observed = self
                    .recover_computer_project_registration(params, &expected, &observed)
                    .await?
                    .0;
            }
        }

        // A validated SQL projection need not be published into the read cache
        // while unrelated writes are pending. Use the returned snapshot as the
        // controlled replacement expectation instead of rereading that cache.
        if let Some(previous) = if refreshed {
            current_registration
        } else {
            self.projects.get(project_id)
        } {
            let previous_container = previous.container_info();
            if AgentRegistrationChange::between(previous_container.as_ref(), &observed).is_some() {
                ensure_request_owner(&previous, &incoming)?;
                let (replacement, retired) = self
                    .recover_computer_project_registration(params, &previous, &observed)
                    .await?;
                observed = replacement;
                retired_request_registration |= retired;
            }
        }

        // Merge request metadata into the current project, rather than copying
        // an old whole-project snapshot across an awaited runtime observation.
        // A concurrent structural publication is retried from its winning view.
        for _ in 0..3 {
            let before = self.projects.get(project_id);
            let mut updated = before
                .as_ref()
                .map(|current| (**current).clone())
                .unwrap_or_else(|| {
                    let mut requested = (*incoming).clone();
                    if retired_request_registration {
                        // Pod ensure may carry a clone of the old registration.
                        // The new request must not resurrect its retired epoch or
                        // sessions after the store proves peer deletion.
                        requested.clear_all_sessions();
                        requested.set_persistence_identity(Default::default());
                    }
                    requested
                });
            if let Some(current) = before.as_ref() {
                ensure_request_owner(current, &incoming)?;
            }
            if updated.user_id().is_none() {
                updated.set_user_id(incoming.user_id().map(str::to_owned));
            }
            if updated.pod_id().is_none() {
                updated.set_pod_id(incoming.pod_id().map(str::to_owned));
            }
            match metadata {
                RegistrationMetadata::PreserveChatMetadata => {
                    updated.set_container(Some(observed.clone()));
                }
                RegistrationMetadata::UpdateChatMetadata => {
                    updated.update_extended_from_request(
                        Some(observed.clone()),
                        incoming.model_provider().cloned(),
                        incoming.request_id().map(str::to_owned),
                        incoming.service_type(),
                    );
                }
            }
            updated.update_activity();
            match self
                .projects
                .insert_project_registration_durable(project_id.to_owned(), Arc::new(updated))
                .await
            {
                Ok(PersistenceWriteOutcome::Committed) => {
                    let registered = self
                        .projects
                        .get(project_id)
                        .context("Computer project was retired during registration")?;
                    ensure!(
                        registered.container_info().is_some_and(|basic| {
                            basic.container_id == observed.container_id
                                && basic.workload_uid == observed.workload_uid
                        }),
                        "Computer runtime changed during registration; retry ensure"
                    );
                    return Ok(registered);
                }
                Ok(PersistenceWriteOutcome::Deferred { reason }) => {
                    return Err(AppError::with_message(
                        shared_types::ERR_SERVICE_UNAVAILABLE,
                        "Computer registration is pending; no chat task was dispatched",
                    )
                    .with_error_detail(
                        ErrorDetail::new(
                            shared_types::ERR_SERVICE_UNAVAILABLE,
                            "computer.registration",
                            reason,
                        )
                        .with_retryable(true),
                    )
                    .into());
                }
                Ok(PersistenceWriteOutcome::Superseded) => {
                    // A fresh request may have reached a replica caching the
                    // project epoch retired by clear-all. Only the store's
                    // proven same-physical refresh may rebase before dispatch.
                    if let Some(refreshed) = self
                        .projects
                        .refresh_project_registration(project_id)
                        .await?
                        && refreshed.container_info().is_some_and(|basic| {
                            basic.container_id == observed.container_id
                                && basic.workload_uid == observed.workload_uid
                        })
                    {
                        ensure_request_owner(&refreshed, &incoming)?;
                        continue;
                    }
                    return Err(AppError::conflict(
                        "Computer registration was retired or replaced before task dispatch",
                    )
                    .into());
                }
                Err(error) => {
                    let after = self.projects.get(project_id);
                    let metadata_raced =
                        before
                            .as_ref()
                            .zip(after.as_ref())
                            .is_some_and(|(before, after)| {
                                !Arc::ptr_eq(before, after)
                                    && after.container_info().is_some_and(|basic| {
                                        basic.container_id == observed.container_id
                                            && basic.workload_uid == observed.workload_uid
                                    })
                            });
                    if !metadata_raced {
                        return Err(error);
                    }
                }
            }
        }
        bail!("Computer project metadata is changing concurrently; retry ensure")
    }

    async fn refresh_inconsistent_computer_registration(
        &self,
        project_id: &str,
    ) -> Result<(Option<Arc<ProjectAndContainerInfo>>, bool)> {
        let current = self.projects.get(project_id);
        if !current
            .as_ref()
            .is_some_and(|info| inconsistent_physical_registration(info))
        {
            return Ok((current, false));
        }
        let current = self
            .projects
            .refresh_project_registration(project_id)
            .await
            .context("Refresh Computer registration after its physical projection changed")?;
        if current
            .as_ref()
            .is_some_and(|info| inconsistent_physical_registration(info))
        {
            return Err(AppError::with_message(
                shared_types::ERR_SERVICE_UNAVAILABLE,
                "Computer registration is changing; no chat task was dispatched",
            )
            .with_error_detail(
                ErrorDetail::new(
                    shared_types::ERR_SERVICE_UNAVAILABLE,
                    "computer.registration",
                    "The current physical projection could not be confirmed from storage",
                )
                .with_retryable(true),
            )
            .into());
        }
        Ok((current, true))
    }

    async fn recover_computer_project_registration(
        &self,
        params: &ContainerCreateParams,
        expected: &ProjectAndContainerInfo,
        observed: &ContainerBasicInfo,
    ) -> Result<(ContainerBasicInfo, bool)> {
        // A peer's durable stop can retire BOTH project and container while this
        // replica still holds the complete old projection. The replacement SQL
        // transaction cannot recover a project row that no longer exists. Refresh
        // its authority first; only the store's exact tombstone-backed retirement
        // may clear the stale mirror and let the caller register the fresh request.
        let refreshed;
        let expected =
            match AgentRegistrationChange::between(expected.container_info().as_ref(), observed) {
                Some(AgentRegistrationChange::WorkloadReplaced) => {
                    refreshed = self
                        .projects
                        .refresh_project_registration(expected.project_id())
                        .await
                        .context("Refresh Computer registration before workload replacement")?;
                    match refreshed.as_ref() {
                        Some(current) => {
                            ensure_request_owner(current, expected)?;
                            if current.container_info().is_some_and(|basic| {
                                basic.container_id == observed.container_id
                                    && basic.workload_uid == observed.workload_uid
                            }) {
                                return Ok((observed.clone(), false));
                            }
                            current.as_ref()
                        }
                        None if self.projects.get(expected.project_id()).is_none() => {
                            return Ok((observed.clone(), true));
                        }
                        None => expected,
                    }
                }
                Some(AgentRegistrationChange::Attached | AgentRegistrationChange::PodRecreated)
                | None => expected,
            };
        let previous = expected.container_info();
        let evidence = self
            .runtime()
            .verify_agent_replacement(params, previous.as_ref(), observed)
            .await
            .map_err(|error| replacement_verification_error(error, previous.as_ref(), observed))
            .context("Observe Computer workload replacement")?;
        let observed = evidence.replacement.clone();
        match self
            .projects
            .replace_agent_container_registration_durable(expected, &evidence)
            .await?
        {
            PersistenceWriteOutcome::Committed => {
                tracing::info!(project_id = %expected.project_id(),
                    old_container_id = ?previous.as_ref().map(|basic| &basic.container_id),
                    container_id = %observed.container_id,
                    "Computer project registration recovered after workload replacement");
                if let Some(previous) = previous.as_ref() {
                    self.teardown_container_connections_guarded(
                        &previous.container_name,
                        &previous.container_ip,
                        Some(&previous.container_id),
                    )
                    .await;
                }
            }
            PersistenceWriteOutcome::Superseded => ensure!(
                self.projects
                    .get(expected.project_id())
                    .is_some_and(|current| {
                        current.container_info().is_some_and(|basic| {
                            basic.container_id == observed.container_id
                                && basic.workload_uid == observed.workload_uid
                        })
                    }),
                "Computer registration changed during replacement; retry ensure"
            ),
            PersistenceWriteOutcome::Deferred { .. } => {
                bail!("Computer replacement registration is not yet committed; retry ensure");
            }
        }
        Ok((observed, false))
    }

    pub async fn append_session_for_container_durable(
        &self,
        expected: &ProjectAndContainerInfo,
        session_id: &str,
    ) -> Result<PersistenceWriteOutcome> {
        self.projects
            .append_session_for_container_durable(expected, session_id)
            .await
    }

    pub async fn get_by_session_with_fetch_result(
        &self,
        session_id: &str,
    ) -> Result<Option<Arc<ProjectAndContainerInfo>>> {
        self.projects
            .get_by_session_id_with_fetch_result(session_id)
            .await
    }

    pub async fn is_session_registration_committed(
        &self,
        expected: &ProjectAndContainerInfo,
        session_id: &str,
    ) -> Result<bool> {
        self.projects
            .is_session_registration_committed(expected, session_id)
            .await
    }
}

/// Keep transport/configuration failures intact. A runtime rejection of an
/// observed controller replacement shares the durable registration gate's
/// typed conflict contract; Pod recreation and attachment keep their semantics.
fn replacement_verification_error(
    error: ContainerRuntimeError,
    previous: Option<&ContainerBasicInfo>,
    observed: &ContainerBasicInfo,
) -> anyhow::Error {
    let conflict = match &error {
        ContainerRuntimeError::Conflict(_) => true,
        ContainerRuntimeError::CreationCancelled
        | ContainerRuntimeError::OperationInProgress(_)
        | ContainerRuntimeError::RequestRejected(_)
        | ContainerRuntimeError::PreparationFailed(_)
        | ContainerRuntimeError::ConnectionError(_)
        | ContainerRuntimeError::ContainerCreationError(_)
        | ContainerRuntimeError::DockerCreationIncomplete { .. }
        | ContainerRuntimeError::ContainerStartError(_)
        | ContainerRuntimeError::ContainerStopError(_)
        | ContainerRuntimeError::ManagementNotRunning
        | ContainerRuntimeError::ContainerNotFound(_)
        | ContainerRuntimeError::ConfigurationError(_)
        | ContainerRuntimeError::Timeout(_)
        | ContainerRuntimeError::K8sError(_)
        | ContainerRuntimeError::DockerError(_)
        | ContainerRuntimeError::ContainerExecError(_)
        | ContainerRuntimeError::CreationAborted { .. } => false,
    };
    let workload_replacement = match AgentRegistrationChange::between(previous, observed) {
        Some(AgentRegistrationChange::WorkloadReplaced) => previous
            .and_then(|previous| previous.workload_uid.as_ref())
            .zip(observed.workload_uid.as_ref())
            .map(|(registered, observed_uid)| ContainerWorkloadReplaced {
                container_name: observed.container_name.clone(),
                registered_workload_uid: registered.clone(),
                observed_workload_uid: observed_uid.clone(),
            }),
        Some(AgentRegistrationChange::Attached | AgentRegistrationChange::PodRecreated) | None => {
            None
        }
    };
    let error = anyhow::Error::new(error);
    if conflict && let Some(replacement) = workload_replacement {
        // Typed context remains downcastable at the HTTP boundary while the
        // original runtime error and its cause remain available to diagnostics.
        error.context(replacement)
    } else {
        error
    }
}

fn inconsistent_physical_registration(project: &ProjectAndContainerInfo) -> bool {
    project.container_info().is_some_and(|basic| {
        project
            .persistence_identity()
            .container
            .as_ref()
            .and_then(|identity| identity.physical_uid.as_deref())
            .is_some_and(|physical| physical != basic.container_id)
    })
}

fn ensure_request_owner(
    registered: &ProjectAndContainerInfo,
    incoming: &ProjectAndContainerInfo,
) -> std::result::Result<(), ComputerOwnerMismatch> {
    let mismatch = if registered
        .user_id()
        .is_some_and(|owner| Some(owner) != incoming.user_id())
    {
        Some(ComputerOwnerMismatch::User)
    } else if registered
        .pod_id()
        .is_some_and(|owner| Some(owner) != incoming.pod_id())
    {
        Some(ComputerOwnerMismatch::Pod)
    } else if registered.service_type().is_some_and(|kind| {
        Some(kind.family_representative())
            != incoming
                .service_type()
                .map(|kind| kind.family_representative())
    }) {
        Some(ComputerOwnerMismatch::ServiceFamily)
    } else {
        None
    };
    if let Some(mismatch) = mismatch {
        tracing::warn!(
            project_id = %incoming.project_id(),
            request_id = ?incoming.request_id(),
            dimension = mismatch.dimension(),
            reason_code = mismatch.reason_code(),
            requested_user_id = ?incoming.user_id(),
            registered_user_id = ?registered.user_id(),
            requested_pod_id = ?incoming.pod_id(),
            registered_pod_id = ?registered.pod_id(),
            requested_service_family = ?incoming.service_type().map(|kind| kind.family_representative()),
            registered_service_family = ?registered.service_type().map(|kind| kind.family_representative()),
            dispatched = false,
            "Computer registration owner conflict before dispatch"
        );
        return Err(mismatch);
    }
    Ok(())
}

#[cfg(test)]
mod owner_tests {
    use super::*;
    use shared_types::ServiceType;

    fn owner() -> ProjectAndContainerInfo {
        let mut info = ProjectAndContainerInfo::new("owner-contract".into());
        info.set_user_id(Some("registered-user".into()));
        info.set_pod_id(Some("registered-pod".into()));
        info.set_service_type(Some(ServiceType::ComputerAgentRunner));
        info
    }

    #[test]
    fn computer_registration_owner_conflicts_keep_distinct_typed_dimensions() {
        let registered = owner();
        let mut incoming = owner();
        incoming.set_user_id(Some("different-user".into()));
        assert_eq!(
            ensure_request_owner(&registered, &incoming),
            Err(ComputerOwnerMismatch::User)
        );
        incoming = owner();
        incoming.set_pod_id(Some("different-pod".into()));
        assert_eq!(
            ensure_request_owner(&registered, &incoming),
            Err(ComputerOwnerMismatch::Pod)
        );
        incoming = owner();
        incoming.set_service_type(Some(ServiceType::WebAgentRunner));
        assert_eq!(
            ensure_request_owner(&registered, &incoming),
            Err(ComputerOwnerMismatch::ServiceFamily)
        );
    }

    #[test]
    fn computer_registration_owner_admission_preserves_optional_fields_and_family_equivalence() {
        let mut registered = owner();
        let mut incoming = owner();
        incoming.set_service_type(Some(ServiceType::ComputerNormalProject));
        assert_eq!(ensure_request_owner(&registered, &incoming), Ok(()));
        registered.set_pod_id(None);
        assert_eq!(ensure_request_owner(&registered, &incoming), Ok(()));
        // A fresh projection really has no registered family; passing None to
        // the setter intentionally preserves a previously known family.
        registered = ProjectAndContainerInfo::new("owner-contract".into());
        assert_eq!(registered.user_id(), None);
        assert_eq!(registered.pod_id(), None);
        assert_eq!(registered.service_type(), None);
        incoming.set_user_id(Some("new-user".into()));
        incoming.set_pod_id(Some("new-pod".into()));
        assert_eq!(ensure_request_owner(&registered, &incoming), Ok(()));
    }

    #[test]
    fn computer_registration_owner_missing_incoming_field_still_conflicts() {
        let registered = owner();
        let mut incoming = owner();
        incoming.set_user_id(None);
        assert_eq!(
            ensure_request_owner(&registered, &incoming),
            Err(ComputerOwnerMismatch::User)
        );
        incoming = owner();
        incoming.set_pod_id(None);
        assert_eq!(
            ensure_request_owner(&registered, &incoming),
            Err(ComputerOwnerMismatch::Pod)
        );
        incoming = ProjectAndContainerInfo::new("owner-contract".into());
        incoming.set_user_id(registered.user_id().map(str::to_owned));
        incoming.set_pod_id(registered.pod_id().map(str::to_owned));
        assert_eq!(incoming.service_type(), None);
        assert_eq!(
            ensure_request_owner(&registered, &incoming),
            Err(ComputerOwnerMismatch::ServiceFamily)
        );
    }

    #[test]
    fn computer_registration_owner_none_setter_preserves_an_existing_family() {
        let registered = owner();
        let mut incoming = owner();
        incoming.set_service_type(None);
        assert_eq!(
            incoming.service_type(),
            Some(ServiceType::ComputerAgentRunner)
        );
        assert_eq!(ensure_request_owner(&registered, &incoming), Ok(()));
        incoming.set_service_type(Some(ServiceType::WebAgentRunner));
        incoming.set_service_type(None);
        assert_eq!(incoming.service_type(), Some(ServiceType::WebAgentRunner));
        assert_eq!(
            ensure_request_owner(&registered, &incoming),
            Err(ComputerOwnerMismatch::ServiceFamily)
        );
    }
}

#[cfg(all(test, feature = "rcoder-pg", feature = "userapp-turso"))]
#[path = "agent_registration/peer_retirement_tests.rs"]
mod peer_retirement_tests;
