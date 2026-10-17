//! Computer registration recovery shared by chat and ordinary pod ensure.
//! Runtime observations authorize a replacement; the store owns its atomic
//! generation transition. No resource creation or deletion happens here.

use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use container_runtime_api::ContainerCreateParams;
use shared_types::{
    AgentRegistrationChange, AppError, ContainerBasicInfo, ErrorDetail, ProjectAndContainerInfo,
    ProjectStore as _, persistence::PersistenceWriteOutcome,
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
        if refreshed && current_registration.is_none() {
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
            let expected = if inconsistent_physical_registration(&expected) {
                self.refresh_inconsistent_computer_registration(expected.project_id())
                    .await?
                    .0
                    .ok_or_else(|| {
                        AppError::conflict(
                            "Canonical Computer registration was retired during refresh",
                        )
                    })?
            } else {
                expected
            };
            if AgentRegistrationChange::between(expected.container_info().as_ref(), &observed)
                .is_some()
            {
                observed = self
                    .recover_computer_project_registration(params, &expected, &observed)
                    .await?;
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
                observed = self
                    .recover_computer_project_registration(params, &previous, &observed)
                    .await?;
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
                .unwrap_or_else(|| (*incoming).clone());
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
    ) -> Result<ContainerBasicInfo> {
        let previous = expected.container_info();
        let evidence = self
            .runtime()
            .verify_agent_replacement(params, previous.as_ref(), observed)
            .await
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
        Ok(observed)
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
) -> Result<()> {
    ensure!(
        registered
            .user_id()
            .is_none_or(|owner| Some(owner) == incoming.user_id())
            && registered
                .pod_id()
                .is_none_or(|owner| Some(owner) == incoming.pod_id())
            && registered.service_type().is_none_or(|kind| {
                Some(kind.family_representative())
                    == incoming
                        .service_type()
                        .map(|kind| kind.family_representative())
            }),
        "Computer project belongs to another registered owner"
    );
    Ok(())
}
