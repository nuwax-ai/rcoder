//! Durable evidence for explicit database password writes. Never includes a password.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DatabasePasswordStage {
    Captured,
    WriteSubmitted,
    Verified,
    /// A committed PG tombstone fences every delayed write of this operation.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", content = "target", rename_all = "snake_case")]
pub enum DatabasePasswordTarget {
    Dev(Box<crate::BuilderControlTarget>),
    Prod(crate::RuntimeConfigurationTarget),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabasePasswordEvidence {
    /// Missing means the writer predates transactional receipts and cannot be
    /// fenced by a cancellation tombstone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_protocol: Option<u32>,
    pub context: crate::UserAppExecutionContext,
    pub username: String,
    pub target: DatabasePasswordTarget,
    pub stage: DatabasePasswordStage,
}

impl DatabasePasswordEvidence {
    pub fn validate_operation(
        &self,
        operation: &crate::UserAppOperationRecord,
    ) -> Result<(), String> {
        self.context.validate_identity(&operation.app_id)?;
        if self.receipt_protocol.is_some_and(|version| version != 1) {
            return Err("Unknown database password receipt protocol".into());
        }
        if self.context.lifecycle_id != operation.lifecycle_id
            || self.context.operation_id != operation.operation_id
            || self.context.request_fingerprint != operation.request_fingerprint
            || operation.executor_id.as_deref() != Some(&self.context.executor_id)
        {
            return Err("Database password evidence has a different execution identity".into());
        }
        let Some(crate::UserAppControlCommand::ResetDatabasePassword {
            production,
            username,
        }) = &operation.command
        else {
            return Err("Database password evidence requires its original command".into());
        };
        crate::pg_utils::validate_pg_identifier(&self.username)?;
        if username != &self.username
            || operation.kind
                != operation
                    .command
                    .as_ref()
                    .map(|command| command.kind())
                    .ok_or("Missing database command")?
        {
            return Err("Database password target account differs from admission".into());
        }
        match &self.target {
            DatabasePasswordTarget::Dev(target)
                if !production && operation.scope == crate::UserAppOperationScope::Dev =>
            {
                target.validate()?;
                if target.context != self.context
                    || target.workload.is_none()
                    || target.workload.as_ref().is_some_and(|workload| {
                        workload.kind == crate::AppResourceKind::StatefulSet && target.pod.is_none()
                    })
                {
                    return Err(
                        "Database password target requires a captured builder and physical pod"
                            .into(),
                    );
                }
            }
            DatabasePasswordTarget::Prod(target)
                if *production && operation.scope == crate::UserAppOperationScope::Prod =>
            {
                if target.physical_uid.is_empty() || target.deployment_generation.is_empty() {
                    return Err(
                        "Database password target requires a physical identity and generation"
                            .into(),
                    );
                }
            }
            _ => return Err("Database password target scope differs from admission".into()),
        }
        Ok(())
    }
}

/// Explicit deployment `pg` input write evidence. The deployment operation owns
/// the receipt transaction, so recovery confirms the original SQL result under
/// the original operation identity without reissuing a password write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplicitDeploymentPasswordEvidence {
    /// Missing means the write predates transactional receipts and cannot be
    /// fenced by a cancellation tombstone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_protocol: Option<u32>,
    pub context: crate::UserAppExecutionContext,
    pub username: String,
    pub explicit_pg_target: crate::RuntimeConfigurationTarget,
    pub stage: DatabasePasswordStage,
}

impl ExplicitDeploymentPasswordEvidence {
    pub fn validate_operation(
        &self,
        operation: &crate::UserAppOperationRecord,
    ) -> Result<(), String> {
        self.context.validate_identity(&operation.app_id)?;
        if self.receipt_protocol.is_some_and(|version| version != 1) {
            return Err("Unknown deployment password receipt protocol".into());
        }
        if self.context.lifecycle_id != operation.lifecycle_id
            || self.context.operation_id != operation.operation_id
            || self.context.request_fingerprint != operation.request_fingerprint
            || operation.executor_id.as_deref() != Some(&self.context.executor_id)
        {
            return Err("Deployment password evidence has a different execution identity".into());
        }
        crate::pg_utils::validate_pg_identifier(&self.username)?;
        let Some(crate::UserAppControlCommand::Deploy { .. }) = &operation.command else {
            return Err("Deployment password evidence requires its original command".into());
        };
        if !matches!(
            operation.kind,
            crate::UserAppOperationKind::StartDeployment
                | crate::UserAppOperationKind::RestartDeployment
        ) || operation.kind
            != operation
                .command
                .as_ref()
                .map(|command| command.kind())
                .ok_or("Missing deployment command")?
            || operation.scope != crate::UserAppOperationScope::Prod
        {
            return Err("Deployment password evidence differs from admission".into());
        }
        if self.explicit_pg_target.physical_uid.is_empty()
            || self.explicit_pg_target.deployment_generation.is_empty()
        {
            return Err(
                "Deployment password target requires a physical identity and generation".into(),
            );
        }
        Ok(())
    }
}

/// Management preparation is not application startup and never promotes a
/// pending runtime configuration. No password is required or stored here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabasePreparationStage {
    Captured,
    StartSubmitted,
    ManagementReady,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabasePreparationEvidence {
    pub target: crate::UserAppMutationTarget,
    pub deployment_generation: String,
    pub stage: DatabasePreparationStage,
    pub management: Option<crate::RuntimeConfigurationTarget>,
}

impl DatabasePreparationEvidence {
    pub fn validate_operation(
        &self,
        operation: &crate::UserAppOperationRecord,
    ) -> Result<(), String> {
        let context = &self.target.context;
        context.validate_identity(&operation.app_id)?;
        if operation.kind != crate::UserAppOperationKind::PrepareProdDatabase
            || operation.command != Some(crate::UserAppControlCommand::PrepareProdDatabase)
            || operation.scope != crate::UserAppOperationScope::Prod
            || context.lifecycle_id != operation.lifecycle_id
            || context.operation_id != operation.operation_id
            || context.request_fingerprint != operation.request_fingerprint
            || operation.executor_id.as_deref() != Some(&context.executor_id)
        {
            return Err("Database preparation execution identity mismatch".into());
        }
        let resource = &self.target.resource;
        if resource.uid.is_empty()
            || resource.name.is_empty()
            || self.deployment_generation.is_empty()
            || !matches!(
                resource.kind,
                crate::AppResourceKind::Container | crate::AppResourceKind::Deployment
            )
            || (resource.kind == crate::AppResourceKind::Deployment
                && resource
                    .resource_version
                    .as_ref()
                    .is_none_or(String::is_empty))
        {
            return Err("Database preparation requires a captured physical workload".into());
        }
        match (&self.stage, &self.management) {
            (DatabasePreparationStage::ManagementReady, Some(management))
                if !management.physical_uid.is_empty()
                    && management.deployment_generation == self.deployment_generation
                    && (resource.kind != crate::AppResourceKind::Container
                        || management.physical_uid == resource.uid) =>
            {
                Ok(())
            }
            (
                DatabasePreparationStage::Captured | DatabasePreparationStage::StartSubmitted,
                None,
            ) => Ok(()),
            _ => Err("Database preparation management evidence is incomplete".into()),
        }
    }
}
