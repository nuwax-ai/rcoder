//! Registration retirement is scoped to the original physical deletion target.
use super::ProjectAdapter;
use anyhow::{Result, ensure};
use shared_types::{AppResourceKind, BuilderControlTarget, ProjectAndContainerInfo, ServiceType};

pub(crate) struct BuilderPhysicalRegistration<'a> {
    pub name: &'a str,
    pub uid: Option<&'a str>,
    pub workload_uid: Option<&'a str>,
}

pub(crate) fn validate_builder_registration_retirement(
    project_id: &str,
    family: Option<ServiceType>,
    physical: Option<BuilderPhysicalRegistration<'_>>,
    target: &BuilderControlTarget,
    original: Option<&shared_types::BuilderRegistryIdentity>,
) -> Result<()> {
    target
        .context
        .validate_identity(&target.context.app_id)
        .map_err(anyhow::Error::msg)?;
    ensure!(
        project_id == target.context.app_id,
        "Builder registration belongs to another application"
    );
    ensure!(
        family.is_none_or(|value| value == ServiceType::UserappBuilder),
        "Builder registration belongs to another service family"
    );
    let Some(physical) = physical else {
        return Ok(());
    };
    let Some(workload) = target.workload.as_ref() else {
        // The original guarded capture found no compute. Only its exact frozen
        // historical route (or an unbound UID) may retire; a later P3 remains.
        ensure!(
            physical
                .uid
                .is_none_or(|uid| original.is_some_and(|captured| captured.container_id == uid)),
            "Builder registration appeared after the empty deletion capture"
        );
        return Ok(());
    };
    ensure!(
        physical.name == workload.name,
        "Builder registration physical scope changed during cleanup"
    );
    // Missing old UID fields and logical_id are history, not new executions.
    // Explicit physical replacements still conflict with the captured target.
    match workload.kind {
        AppResourceKind::Container => ensure!(
            physical.uid.is_none_or(|uid| uid == workload.uid
                || original.is_some_and(|captured| captured.container_id == uid)),
            "Builder container was replaced during cleanup"
        ),
        AppResourceKind::StatefulSet => {
            ensure!(
                physical.workload_uid.is_none_or(|uid| uid == workload.uid),
                "Builder workload was replaced during cleanup"
            );
            if let Some(pod) = &target.pod {
                ensure!(
                    physical.uid.is_none_or(|uid| uid == pod.uid
                        || original.is_some_and(|captured| captured.container_id == uid)),
                    "Builder Pod was replaced during cleanup"
                );
            }
        }
        AppResourceKind::Deployment
        | AppResourceKind::Service
        | AppResourceKind::ConfigMap
        | AppResourceKind::Secret
        | AppResourceKind::HttpRoute
        | AppResourceKind::PersistentVolumeClaim
        | AppResourceKind::HostPath
        | AppResourceKind::File => anyhow::bail!("Builder deletion target is not compute"),
    }
    Ok(())
}

fn validate_builder_retirement(
    current: &ProjectAndContainerInfo,
    target: &BuilderControlTarget,
    original: Option<&shared_types::BuilderRegistryIdentity>,
) -> Result<()> {
    let physical = current.container_info();
    validate_builder_registration_retirement(
        current.project_id(),
        current.service_type(),
        physical.as_ref().map(|basic| BuilderPhysicalRegistration {
            name: &basic.container_name,
            uid: (!basic.container_id.is_empty()).then_some(basic.container_id.as_str()),
            workload_uid: basic.workload_uid.as_deref().filter(|uid| !uid.is_empty()),
        }),
        target,
        original,
    )
}

impl ProjectAdapter {
    pub(crate) fn capture_builder_registry_identity(
        &self,
        app_id: &str,
    ) -> Result<Option<shared_types::BuilderRegistryIdentity>> {
        let _project_guard = self.project_locks.entry_by_ref(app_id);
        let Some(project) = self.get(app_id) else {
            return Ok(None);
        };
        ensure!(
            project
                .service_type()
                .is_none_or(|family| family == ServiceType::UserappBuilder),
            "Builder registration belongs to another service family"
        );
        Ok(project
            .container_info()
            .filter(|container| !container.container_id.is_empty())
            .map(|container| shared_types::BuilderRegistryIdentity {
                generation: project.persistence_identity().generation.clone(),
                container_id: container.container_id,
            }))
    }

    pub(crate) fn check_captured_builder_registration(
        &self,
        target: &BuilderControlTarget,
        original: Option<&shared_types::BuilderRegistryIdentity>,
    ) -> Result<()> {
        self.captured_builder_registration(target, original, false)
    }

    pub(crate) fn retire_captured_builder_registration(
        &self,
        target: &BuilderControlTarget,
        original: Option<&shared_types::BuilderRegistryIdentity>,
    ) -> Result<()> {
        self.captured_builder_registration(target, original, true)
    }

    fn captured_builder_registration(
        &self,
        target: &BuilderControlTarget,
        original: Option<&shared_types::BuilderRegistryIdentity>,
        retire: bool,
    ) -> Result<()> {
        let project = &target.context.app_id;
        let _project_guard = self.project_locks.entry_by_ref(project);
        let _container_guard = target
            .workload
            .as_ref()
            .map(|resource| self.container_locks.entry_by_ref(&resource.name));
        let actual_uid = target.pod.as_ref().map(|pod| pod.uid.as_str()).or_else(|| {
            target
                .workload
                .as_ref()
                .and_then(|workload| match workload.kind {
                    AppResourceKind::Container => Some(workload.uid.as_str()),
                    AppResourceKind::StatefulSet
                    | AppResourceKind::Deployment
                    | AppResourceKind::Service
                    | AppResourceKind::ConfigMap
                    | AppResourceKind::Secret
                    | AppResourceKind::HttpRoute
                    | AppResourceKind::PersistentVolumeClaim
                    | AppResourceKind::HostPath
                    | AppResourceKind::File => None,
                })
        });
        ensure!(
            self.iter().iter().all(|(id, other)| id == project
                || other.container_info().is_none_or(|physical| target
                    .workload
                    .as_ref()
                    .is_none_or(|workload| physical.container_name != workload.name)
                    && actual_uid.is_none_or(|uid| physical.container_id != uid))),
            "Captured physical builder has shared references"
        );
        let Some(current) = self.get(project) else {
            return Ok(());
        };
        validate_builder_retirement(&current, target, original)?;
        if let Some(basic) = current.container_info() {
            ensure!(
                self.iter().iter().all(|(id, other)| id == project
                    || other
                        .container_info()
                        .is_none_or(|physical| physical.container_name != basic.container_name
                            && (basic.container_id.is_empty()
                                || physical.container_id != basic.container_id))),
                "Captured builder registration has shared references"
            );
        }
        if retire {
            self.remove_locked(project);
        }
        Ok(())
    }
}
