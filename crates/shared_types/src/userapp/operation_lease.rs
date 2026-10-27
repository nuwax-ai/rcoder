//! Durable receipt of the exact operation mutex, independent of business input.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "runtime", rename_all = "snake_case", deny_unknown_fields)]
pub enum UserAppOperationLeaseReceipt {
    Docker {
        service_type: crate::ServiceType,
        device: u64,
        inode: u64,
        token: String,
    },
    Kubernetes {
        service_type: crate::ServiceType,
        namespace: String,
        name: String,
        uid: String,
        resource_version: String,
        token: String,
    },
}
impl UserAppOperationLeaseReceipt {
    pub fn service_type(&self) -> &crate::ServiceType {
        match self {
            Self::Docker { service_type, .. } | Self::Kubernetes { service_type, .. } => {
                service_type
            }
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if !matches!(
            self.service_type(),
            crate::ServiceType::Userapp | crate::ServiceType::UserappBuilder
        ) {
            return Err("Invalid operation lease family".into());
        }
        let token = match self {
            Self::Docker { inode, token, .. } if *inode != 0 => token,
            Self::Kubernetes {
                namespace,
                name,
                uid,
                resource_version,
                token,
                ..
            } if !namespace.is_empty()
                && !name.is_empty()
                && !uid.is_empty()
                && !resource_version.is_empty() =>
            {
                token
            }
            _ => return Err("Incomplete operation lease receipt".into()),
        };
        crate::validate_identifier(token, "lease token")
            .map_err(|_| "Invalid operation lease token".into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAppOperationLeaseBinding {
    pub context: crate::UserAppExecutionContext,
    pub receipt: UserAppOperationLeaseReceipt,
}

/// Creation may have returned before the control-store checkpoint was saved.
/// Only a runtime receipt for this exact executor can resolve these stages.
pub fn userapp_builder_creation_needs_runtime_receipt(op: &crate::UserAppOperationRecord) -> bool {
    op.kind == crate::UserAppOperationKind::EnsureBuilder
        && matches!(
            op.state,
            crate::UserAppOperationState::Running | crate::UserAppOperationState::RecoveryRequired
        )
        && op.executor_id.is_some()
        && (op.checkpoint.is_null()
            || op.checkpoint.as_object().is_some_and(|checkpoint| {
                checkpoint.len() == 1
                    && checkpoint
                        .get("builder_creation_predecessor")
                        .is_some_and(|value| {
                            serde_json::from_value::<crate::BuilderCreationPredecessor>(
                                value.clone(),
                            )
                            .is_ok_and(|source| source.validate_operation(op).is_ok())
                        })
            }))
        && matches!(
            op.step.as_str(),
            "claimed"
                | "creation_source_captured"
                | "creation_confirmation_timed_out"
                | "worker_interrupted"
                | "creation_result"
        )
}

/// Evidence sufficient to drain an interrupted writer. A returned creation
/// request with its runtime lease released need not have a healthy application
/// endpoint: Stop must still be able to stop that confirmed physical instance.
/// This predicate must never authorize a successful business result.
pub fn userapp_operation_has_drain_evidence(operation: &crate::UserAppOperationRecord) -> bool {
    userapp_operation_has_final_evidence(operation)
        || (operation.kind == crate::UserAppOperationKind::EnsureBuilder
            && operation.step == "builder_created_observed"
            && serde_json::from_value::<crate::BuilderCreationEvidence>(
                operation.checkpoint.clone(),
            )
            .is_ok_and(|evidence| evidence.validate_operation(operation).is_ok()))
}

/// A returned configuration update, not completion of the whole deployment.
/// The expiry transaction must additionally verify the original private input
/// with `userapp_deployment_update_precedes_credentials` before retiring it.
pub fn userapp_operation_has_deployment_update_receipt(
    operation: &crate::UserAppOperationRecord,
) -> bool {
    let Some(command @ crate::UserAppControlCommand::Deploy { .. }) = operation.command.as_ref()
    else {
        return false;
    };
    if command.kind() != operation.kind
        || operation.scope != crate::UserAppOperationScope::Prod
        || operation.step != "runtime_updated"
        || operation.checkpoint.get("storage_target") != Some(&serde_json::Value::Null)
        || operation.checkpoint.get("requested_storage_size") != Some(&serde_json::Value::Null)
    {
        return false;
    }
    let Some(target) = operation.checkpoint.get("target").and_then(|value| {
        serde_json::from_value::<crate::UserAppMutationTarget>(value.clone()).ok()
    }) else {
        return false;
    };
    target.context.app_id == operation.app_id
        && target.context.validate_identity(&operation.app_id).is_ok()
        && target.context.lifecycle_id == operation.lifecycle_id
        && target.context.operation_id == operation.operation_id
        && Some(&target.context.executor_id) == operation.executor_id.as_ref()
        && target.context.request_fingerprint == operation.request_fingerprint
        && target.resource.kind == crate::AppResourceKind::Deployment
        && !target.resource.name.is_empty()
        && !target.resource.uid.is_empty()
        && target
            .resource
            .resource_version
            .as_deref()
            .is_some_and(|value| !value.is_empty())
        && operation
            .checkpoint
            .pointer("/resource/project_id")
            .and_then(|value| value.as_str())
            == Some(operation.app_id.as_str())
        && operation
            .checkpoint
            .pointer("/resource/container_id")
            .and_then(|value| value.as_str())
            == Some(target.resource.name.as_str())
}

/// DeployInput v1 has always persisted `explicit_pg_applying` before issuing
/// the explicit credential write. With pg present, no later deployment/SQL
/// write can bypass that checkpoint. Retiring the exact runtime_updated
/// revision therefore closes future writes without guessing executor liveness.
/// This must never authorize a successful deployment or replay its input.
pub fn userapp_deployment_update_precedes_credentials(
    operation: &crate::UserAppOperationRecord,
    input: &crate::UserAppExecutionInput,
) -> bool {
    if !userapp_operation_has_deployment_update_receipt(operation) {
        return false;
    }
    let Some(crate::UserAppControlCommand::Deploy {
        restart,
        input_digest,
        ..
    }) = operation.command.as_ref()
    else {
        return false;
    };
    if *input_digest != input.digest() {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(input.encoded()) else {
        return false;
    };
    value.get("version").and_then(|value| value.as_u64()) == Some(1)
        && value.get("restart").and_then(|value| value.as_bool()) == Some(*restart)
        && value.get("params").is_some_and(|value| value.is_object())
        && value.get("previous").is_some_and(|value| value.is_object())
        && value.pointer("/request/pg").is_some_and(|value| {
            serde_json::from_value::<crate::StartPgCredential>(value.clone())
                .is_ok_and(|pg| !pg.username.is_empty() && !pg.password.is_empty())
        })
}

/// A final durable checkpoint is proof of completed effects, never a license to
/// repeat earlier I/O. Recovery may only release its original mutex and commit.
pub fn userapp_operation_has_final_evidence(operation: &crate::UserAppOperationRecord) -> bool {
    use crate::UserAppOperationKind as Kind;
    match operation.kind {
        Kind::EnsureBuilder => {
            operation.step == "builder_ready_confirmed"
                && serde_json::from_value::<crate::BuilderCreationEvidence>(
                    operation.checkpoint.clone(),
                )
                .is_ok_and(|evidence| evidence.validate_operation(operation).is_ok())
        }
        Kind::Create => {
            operation.step == "runtime_created"
                && operation
                    .checkpoint
                    .pointer("/resource/container_id")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|value| !value.is_empty())
        }
        Kind::Update => {
            operation.step == "runtime_updated"
                && operation
                    .checkpoint
                    .pointer("/resource/container_id")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|value| !value.is_empty())
        }
        Kind::StartDeployment | Kind::RestartDeployment => {
            operation.step == "deployment_completed"
                && operation
                    .checkpoint
                    .get("operation_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(operation.operation_id.as_str())
                && operation
                    .checkpoint
                    .get("app_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(operation.app_id.as_str())
        }
        Kind::Start | Kind::Restart | Kind::Stop | Kind::SetRecyclePolicy => {
            operation.step == "control_confirmed"
                && operation
                    .checkpoint
                    .pointer("/target/context/operation_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(operation.operation_id.as_str())
        }
        Kind::StopBuilder | Kind::RestartBuilder => {
            operation.step == "compute_confirmed"
                && operation
                    .checkpoint
                    .pointer("/target/context/operation_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(operation.operation_id.as_str())
                && operation
                    .checkpoint
                    .pointer("/result/operation_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(operation.operation_id.as_str())
        }
        Kind::DeleteCompute | Kind::PurgeResources | Kind::DeleteApplication => {
            serde_json::from_value::<crate::UserAppDeletionCheckpoint>(operation.checkpoint.clone())
                .ok()
                .is_some_and(|evidence| {
                    evidence.validate_operation(operation).is_ok()
                        && evidence.stage
                            == if operation.kind == Kind::DeleteCompute {
                                crate::UserAppDeletionStage::ComputeRemoved
                            } else {
                                crate::UserAppDeletionStage::DevelopmentRemoved
                            }
                })
        }
        Kind::DestroyDevStorage | Kind::DestroyProdStorage => {
            operation.step == "development_storage_removed"
                && serde_json::from_value::<crate::UserAppStorageDestruction>(
                    operation.checkpoint.clone(),
                )
                .ok()
                .is_some_and(|evidence| {
                    evidence.validate().is_ok()
                        && evidence.context.operation_id == operation.operation_id
                        && evidence.context.lifecycle_id == operation.lifecycle_id
                        && evidence.context.app_id == operation.app_id
                        && operation.executor_id.as_deref()
                            == Some(evidence.context.executor_id.as_str())
                })
        }
        Kind::ClearDevStorage | Kind::ClearProdStorage => {
            operation.step == "storage_contents_cleared"
                && serde_json::from_value::<crate::UserAppStorageClear>(
                    operation.checkpoint.clone(),
                )
                .ok()
                .is_some_and(|evidence| {
                    evidence.validate().is_ok()
                        && evidence.context.operation_id == operation.operation_id
                        && evidence.context.lifecycle_id == operation.lifecycle_id
                        && evidence.context.app_id == operation.app_id
                        && operation.executor_id.as_deref()
                            == Some(evidence.context.executor_id.as_str())
                })
        }
        Kind::PrepareProdDatabase => serde_json::from_value::<crate::DatabasePreparationEvidence>(
            operation.checkpoint.clone(),
        )
        .is_ok_and(|evidence| {
            evidence.stage == crate::DatabasePreparationStage::ManagementReady
                && evidence.validate_operation(operation).is_ok()
        }),
        Kind::ResetDevDatabasePassword | Kind::ResetProdDatabasePassword => {
            serde_json::from_value::<crate::DatabasePasswordEvidence>(operation.checkpoint.clone())
                .is_ok_and(|evidence| {
                    evidence.stage == crate::DatabasePasswordStage::Verified
                        && evidence.validate_operation(operation).is_ok()
                })
        }
        _ => false,
    }
}
