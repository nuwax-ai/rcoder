//! Pure comparisons for Dev queries. They never authorize a runtime mutation.
use container_runtime_api::{ContainerRuntimeStatus, RuntimeContainerInfo};
use shared_types::{
    AppResourceIdentity, BuilderControlTarget, BuilderPodIdentity, ContainerBasicInfo,
};

/// A normal status update changes resourceVersion without replacing compute.
/// Preserve every other captured field, including bindings and frozen intent.
/// The original target and its versions remain intact for conditional writes.
pub(super) fn same_read_capture(
    before: &BuilderControlTarget,
    after: &BuilderControlTarget,
) -> bool {
    // List every field so new capture fields require an explicit read policy.
    let BuilderControlTarget {
        resource_binding,
        context,
        workload,
        pod,
        restart_image,
        restart_runtime_workspace,
        builder_startup_mode,
    } = before;
    resource_binding == &after.resource_binding
        && context == &after.context
        && same_workload(workload.as_ref(), after.workload.as_ref())
        && same_pod(pod.as_ref(), after.pod.as_ref())
        && restart_image == &after.restart_image
        && restart_runtime_workspace == &after.restart_runtime_workspace
        && builder_startup_mode == &after.builder_startup_mode
}

fn same_workload(
    before: Option<&AppResourceIdentity>,
    after: Option<&AppResourceIdentity>,
) -> bool {
    match (before, after) {
        (Some(before), Some(after)) => {
            let AppResourceIdentity {
                kind,
                name,
                uid,
                resource_version: _,
            } = before;
            kind == &after.kind && name == &after.name && uid == &after.uid
        }
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

fn same_pod(before: Option<&BuilderPodIdentity>, after: Option<&BuilderPodIdentity>) -> bool {
    match (before, after) {
        (Some(before), Some(after)) => {
            let BuilderPodIdentity {
                name,
                uid,
                resource_version: _,
            } = before;
            name == &after.name && uid == &after.uid
        }
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

/// Endpoint equality is separate from readiness and lifecycle verification.
#[derive(PartialEq, Eq)]
struct Endpoint<'a> {
    container_id: &'a str,
    container_name: &'a str,
    workload_uid: Option<&'a str>,
    address: &'a str,
}

impl<'a> From<&'a RuntimeContainerInfo> for Endpoint<'a> {
    fn from(info: &'a RuntimeContainerInfo) -> Self {
        Self {
            container_id: &info.container_id,
            container_name: &info.container_name,
            workload_uid: info.workload_uid.as_deref(),
            address: &info.container_ip,
        }
    }
}

impl<'a> From<&'a ContainerBasicInfo> for Endpoint<'a> {
    fn from(info: &'a ContainerBasicInfo) -> Self {
        Self {
            container_id: &info.container_id,
            container_name: &info.container_name,
            workload_uid: info.workload_uid.as_deref(),
            address: &info.container_ip,
        }
    }
}

pub(super) fn same_discovery_endpoint(
    before: &RuntimeContainerInfo,
    after: &RuntimeContainerInfo,
) -> bool {
    Endpoint::from(before) == Endpoint::from(after)
}

pub(super) fn same_log_endpoint(
    actual: &RuntimeContainerInfo,
    address: &ContainerBasicInfo,
) -> bool {
    Endpoint::from(actual) == Endpoint::from(address)
}

pub(super) fn is_running(status: &ContainerRuntimeStatus) -> bool {
    match status {
        ContainerRuntimeStatus::Running => true,
        ContainerRuntimeStatus::Pending
        | ContainerRuntimeStatus::Succeeded
        | ContainerRuntimeStatus::Failed
        | ContainerRuntimeStatus::Unknown(_) => false,
    }
}
