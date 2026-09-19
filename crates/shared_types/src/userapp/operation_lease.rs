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
