//! Current retained-volume creation and stopped-controller replacement witnesses.
use shared_types::*;

// The runtime proves actual absence/creation and exact retained PVC ownership.
// This store only binds that evidence to the original intent and lease; a
// multi-step creator must never be classified as an RV-fenced single write.
pub(super) fn retained_builder_volume_evidence(record: &ComputeControlRecord) -> bool {
    let Some(volumes) = record.checkpoint.get("builder_volumes") else {
        return false;
    };
    let Ok(volumes) = serde_json::from_value::<Vec<AppResourceIdentity>>(volumes.clone()) else {
        return false;
    };
    let mut names = std::collections::HashSet::new();
    let mut uids = std::collections::HashSet::new();
    record.action == ComputeControlAction::Restart
        && record.scope == UserAppOperationScope::Dev
        && matches!(
            record.lease,
            Some(UserAppOperationLeaseReceipt::Kubernetes {
                service_type: ServiceType::UserappBuilder,
                ..
            })
        )
        && !volumes.is_empty()
        && volumes.iter().all(|volume| {
            volume.kind == AppResourceKind::PersistentVolumeClaim
                && !volume.name.is_empty()
                && !volume.uid.is_empty()
                && names.insert(&volume.name)
                && uids.insert(&volume.uid)
        })
}

pub(super) fn retained_builder_creation(record: &ComputeControlRecord) -> bool {
    serde_json::from_value::<BuilderControlTarget>(record.checkpoint.clone()).is_ok_and(|target| {
        retained_builder_volume_evidence(record)
            && record
                .execution_context()
                .is_ok_and(|context| target.context == context)
            && target.validate().is_ok()
            && target.workload.is_none()
            && target.pod.is_none()
            && target.resource_binding.is_none()
    })
}

// Runtime has confirmed whole-compute absence and the same actual PVC. None
// selects the same canonical dev directory in the Kubernetes factory; an old
// controller UID or restart archive cannot prevent this retained-data recovery.
pub(super) fn stopped_builder_absence(
    record: &ComputeControlRecord,
    next: &BuilderControlTarget,
) -> bool {
    serde_json::from_value::<BuilderControlTarget>(record.checkpoint.clone()).is_ok_and(|old| {
        record.stage == "stopped"
            && retained_builder_volume_evidence(record)
            && old.validate().is_ok()
            && next.validate().is_ok()
            && record
                .execution_context()
                .is_ok_and(|context| old.context == context && next.context == context)
            && old
                .workload
                .as_ref()
                .is_some_and(|resource| resource.kind == AppResourceKind::StatefulSet)
            && next.workload.is_none()
            && next.pod.is_none()
            && next.resource_binding.is_none()
            && old.restart_image == next.restart_image
            && old.builder_startup_mode == next.builder_startup_mode
            && old
                .restart_runtime_workspace
                .as_deref()
                .is_none_or(|workspace| workspace == paths::userapp_dev_workspace(&record.app_id))
            && next.restart_runtime_workspace.is_none()
    })
}
