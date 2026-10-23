//! Closed-enum decisions for compute control. New variants require review here.
use super::*;

pub(super) fn is_restart(action: ComputeControlAction) -> bool {
    match action {
        ComputeControlAction::Stop => false,
        ComputeControlAction::Restart => true,
    }
}

pub(super) fn uses_builder(scope: UserAppOperationScope) -> Result<bool> {
    match scope {
        UserAppOperationScope::Dev => Ok(true),
        UserAppOperationScope::Prod => Ok(false),
        UserAppOperationScope::Application => {
            anyhow::bail!("Compute control requires a Dev or Prod scope")
        }
    }
}

pub(super) fn uses_retained_pvc(kind: AppResourceKind) -> bool {
    match kind {
        AppResourceKind::StatefulSet => true,
        AppResourceKind::Container
        | AppResourceKind::Deployment
        | AppResourceKind::Service
        | AppResourceKind::ConfigMap
        | AppResourceKind::Secret
        | AppResourceKind::HttpRoute
        | AppResourceKind::PersistentVolumeClaim
        | AppResourceKind::HostPath
        | AppResourceKind::File => false,
    }
}

pub(super) fn restart_archive_kind(kind: AppResourceKind) -> Option<AppResourceKind> {
    match kind {
        AppResourceKind::Container => Some(AppResourceKind::File),
        AppResourceKind::StatefulSet => Some(AppResourceKind::Secret),
        AppResourceKind::Deployment
        | AppResourceKind::Service
        | AppResourceKind::ConfigMap
        | AppResourceKind::Secret
        | AppResourceKind::HttpRoute
        | AppResourceKind::PersistentVolumeClaim
        | AppResourceKind::HostPath
        | AppResourceKind::File => None,
    }
}

pub(super) fn is_pvc(kind: AppResourceKind) -> bool {
    match kind {
        AppResourceKind::PersistentVolumeClaim => true,
        AppResourceKind::Container
        | AppResourceKind::Deployment
        | AppResourceKind::StatefulSet
        | AppResourceKind::Service
        | AppResourceKind::ConfigMap
        | AppResourceKind::Secret
        | AppResourceKind::HttpRoute
        | AppResourceKind::HostPath
        | AppResourceKind::File => false,
    }
}

pub(super) fn is_kubernetes_builder_lease(receipt: &UserAppOperationLeaseReceipt) -> bool {
    match receipt {
        UserAppOperationLeaseReceipt::Docker { .. } => false,
        UserAppOperationLeaseReceipt::Kubernetes { service_type, .. } => match service_type {
            ServiceType::UserappBuilder => true,
            ServiceType::WebAgentRunner
            | ServiceType::ComputerAgentRunner
            | ServiceType::ComputerNormalProject
            | ServiceType::Userapp => false,
        },
    }
}

pub(super) fn management_only(mode: BuilderStartupMode) -> bool {
    match mode {
        BuilderStartupMode::PreserveBusinessIntent => false,
        BuilderStartupMode::ManagementOnly => true,
    }
}

pub(super) fn completed_result(state: ComputeControlState) -> bool {
    match state {
        ComputeControlState::Succeeded | ComputeControlState::Failed => true,
        ComputeControlState::Pending
        | ComputeControlState::Running
        | ComputeControlState::RecoveryRequired
        | ComputeControlState::Superseded => false,
    }
}

pub(super) fn write_in_flight(stage: &str) -> bool {
    match ComputeControlStage::from_storage_value(stage) {
        Some(ComputeControlStage::Starting | ComputeControlStage::Stopping) => true,
        Some(
            ComputeControlStage::DrainingPrevious
            | ComputeControlStage::Stopped
            | ComputeControlStage::Verifying
            | ComputeControlStage::Completed,
        )
        | None => false,
    }
}

pub(super) fn persisted_mutation_boundary(stage: &str) -> bool {
    match ComputeControlStage::from_storage_value(stage) {
        Some(ComputeControlStage::Stopped | ComputeControlStage::Verifying) => true,
        Some(
            ComputeControlStage::DrainingPrevious
            | ComputeControlStage::Stopping
            | ComputeControlStage::Starting
            | ComputeControlStage::Completed,
        )
        | None => false,
    }
}

pub(super) fn can_observe_completion(action: ComputeControlAction, stage: &str) -> bool {
    let stage = ComputeControlStage::from_storage_value(stage);
    match action {
        ComputeControlAction::Stop => match stage {
            Some(ComputeControlStage::Stopping | ComputeControlStage::Stopped) => true,
            Some(
                ComputeControlStage::DrainingPrevious
                | ComputeControlStage::Starting
                | ComputeControlStage::Verifying
                | ComputeControlStage::Completed,
            )
            | None => false,
        },
        ComputeControlAction::Restart => match stage {
            Some(ComputeControlStage::Starting | ComputeControlStage::Verifying) => true,
            Some(
                ComputeControlStage::DrainingPrevious
                | ComputeControlStage::Stopping
                | ComputeControlStage::Stopped
                | ComputeControlStage::Completed,
            )
            | None => false,
        },
    }
}

pub(super) fn is_version_conflict(error: &UserAppStoreError) -> bool {
    match error {
        UserAppStoreError::VersionConflict => true,
        UserAppStoreError::OwnershipConflict
        | UserAppStoreError::LifecycleConflict
        | UserAppStoreError::OperationInProgress(_)
        | UserAppStoreError::RequestReplayConflict(_)
        | UserAppStoreError::NotFound
        | UserAppStoreError::InvalidOperation(_)
        | UserAppStoreError::Storage(_) => false,
    }
}

pub(super) fn is_runtime_absence(error: &container_runtime_api::ContainerRuntimeError) -> bool {
    use container_runtime_api::ContainerRuntimeError as Error;
    match error {
        Error::ContainerNotFound(_) => true,
        Error::CreationCancelled
        | Error::OperationInProgress(_)
        | Error::RequestRejected(_)
        | Error::PreparationFailed(_)
        | Error::Conflict(_)
        | Error::ConnectionError(_)
        | Error::ContainerCreationError(_)
        | Error::DockerCreationIncomplete { .. }
        | Error::ContainerStartError(_)
        | Error::ContainerStopError(_)
        | Error::ManagementNotRunning
        | Error::ConfigurationError(_)
        | Error::Timeout(_)
        | Error::K8sError(_)
        | Error::DockerError(_)
        | Error::ContainerExecError(_)
        | Error::CreationAborted { .. } => false,
    }
}
