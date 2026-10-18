//! Verification uses surviving PVCs, without rewriting original discovery history.
use super::{codec, storage};
use crate::db::models;
use shared_types::{
    AppResourceIdentity, AppResourceKind, UserAppDeletionCheckpoint, UserAppDeletionStage,
    UserAppDiscoveredIdentity, UserAppOperationKind, UserAppOperationRecord, UserAppOperationState,
    UserAppStorageClear, UserAppStorageClearTarget, UserAppStorageDestruction, UserAppStoreError,
};
use toasty::Executor;

const RETIREMENT_PAGE_SIZE: usize = 32;

#[cfg(test)]
mod tests;

pub(super) async fn original(
    tx: &mut dyn Executor,
    app_id: &str,
    lifecycle_id: &str,
) -> Result<Option<UserAppDiscoveredIdentity>, UserAppStoreError> {
    let Some(row) =
        models::RecoveryWitness::filter_by_app_id_and_lifecycle_id(app_id, lifecycle_id)
            .first()
            .exec(tx)
            .await
            .map_err(storage)?
    else {
        return Ok(None);
    };
    let witness: UserAppDiscoveredIdentity =
        serde_json::from_str(&row.witness_json).map_err(storage)?;
    witness
        .validate()
        .map_err(UserAppStoreError::InvalidOperation)?;
    if witness.app_id != app_id || witness.lifecycle_id != lifecycle_id {
        return Err(UserAppStoreError::LifecycleConflict);
    }
    Ok(Some(witness))
}

fn same_pvc(expected: &AppResourceIdentity, removed: &AppResourceIdentity) -> bool {
    // Namespace is implicit in the application's runtime. Kubernetes PVC UIDs
    // are immutable: metadata/resourceVersion changes cannot revive a retired UID.
    expected.kind == AppResourceKind::PersistentVolumeClaim
        && removed.kind == AppResourceKind::PersistentVolumeClaim
        && expected.name == removed.name
        && expected.uid == removed.uid
}

fn subtract_resources(
    witness: &mut UserAppDiscoveredIdentity,
    production: &[AppResourceIdentity],
    development: &[AppResourceIdentity],
) {
    witness
        .prod_volumes
        .retain(|expected| !production.iter().any(|removed| same_pvc(expected, removed)));
    witness.dev_volumes.retain(|expected| {
        !development
            .iter()
            .any(|removed| same_pvc(expected, removed))
    });
}

fn subtract_confirmed_retirement(
    witness: &mut UserAppDiscoveredIdentity,
    operation: &UserAppOperationRecord,
) -> Result<(), String> {
    if operation.app_id != witness.app_id
        || operation.lifecycle_id != witness.lifecycle_id
        || operation.kind.scope() != operation.scope
        || operation.state != UserAppOperationState::Succeeded
    {
        return Err("Completed storage removal does not belong to the recovered lifecycle".into());
    }
    // Succeeded is recorded only after the store validates each kind's final
    // removal stage. The generic coordinator then replaces its diagnostic step
    // with "completed"; that label is not an additional historical fence.
    match operation.kind {
        UserAppOperationKind::PurgeResources => {
            let checkpoint: UserAppDeletionCheckpoint =
                serde_json::from_value(operation.checkpoint.clone()).map_err(|error| {
                    format!("Completed purge checkpoint is unreadable: {error}")
                })?;
            checkpoint.validate_operation(operation)?;
            if checkpoint.stage != UserAppDeletionStage::DevelopmentRemoved {
                return Err("Completed purge has no final storage-removal evidence".into());
            }
            let development = checkpoint
                .development
                .as_ref()
                .ok_or_else(|| "Completed purge development receipt is missing".to_owned())?;
            subtract_resources(
                witness,
                &checkpoint.production.resources,
                &development.runtime.resources,
            );
        }
        UserAppOperationKind::DestroyDevStorage | UserAppOperationKind::DestroyProdStorage => {
            let checkpoint: UserAppStorageDestruction =
                serde_json::from_value(operation.checkpoint.clone()).map_err(|error| {
                    format!("Completed storage destruction checkpoint is unreadable: {error}")
                })?;
            checkpoint.validate()?;
            if !super::ops::owns(&checkpoint.context, operation)
                || checkpoint.production.is_some()
                    != (operation.kind == UserAppOperationKind::DestroyProdStorage)
            {
                return Err("Storage destruction has no matching final removal receipt".into());
            }
            subtract_resources(
                witness,
                checkpoint
                    .production
                    .as_ref()
                    .map(|production| production.resources.as_slice())
                    .unwrap_or_default(),
                &checkpoint.development.runtime.resources,
            );
        }
        UserAppOperationKind::ClearProdStorage => {
            let checkpoint: UserAppStorageClear =
                serde_json::from_value(operation.checkpoint.clone()).map_err(|error| {
                    format!("Completed storage clear checkpoint is unreadable: {error}")
                })?;
            checkpoint.validate()?;
            if !super::ops::owns(&checkpoint.context, operation) {
                return Err("Storage clear has no matching final receipt".into());
            }
            match checkpoint.target {
                // Kubernetes production clear deletes captured PVCs. Docker
                // instead retains and clears captured directory leases; that
                // receipt cannot retire a volume merely because clear succeeded.
                UserAppStorageClearTarget::Production {
                    snapshot,
                    directories,
                } if directories.is_empty() && snapshot.directories.is_none() => {
                    subtract_resources(witness, &snapshot.resources, &[]);
                }
                UserAppStorageClearTarget::Production { .. } => {}
                UserAppStorageClearTarget::Development { .. } => {
                    return Err("Production clear receipt contains a development target".into());
                }
            }
        }
        UserAppOperationKind::EnsureBuilder
        | UserAppOperationKind::AdoptBuilder
        | UserAppOperationKind::AdoptApplication
        | UserAppOperationKind::StopBuilder
        | UserAppOperationKind::RestartBuilder
        | UserAppOperationKind::Create
        | UserAppOperationKind::StartDeployment
        | UserAppOperationKind::RestartDeployment
        | UserAppOperationKind::Update
        | UserAppOperationKind::Start
        | UserAppOperationKind::Restart
        | UserAppOperationKind::Stop
        | UserAppOperationKind::SetRecyclePolicy
        | UserAppOperationKind::HotDeploy
        | UserAppOperationKind::DeleteCompute
        | UserAppOperationKind::ClearDevStorage
        | UserAppOperationKind::ResetDevDatabasePassword
        | UserAppOperationKind::ResetProdDatabasePassword
        | UserAppOperationKind::PrepareProdDatabase
        | UserAppOperationKind::DeleteApplication => {
            return Err("Operation does not retire recovered storage volumes".into());
        }
    }
    Ok(())
}

pub(super) async fn remaining(
    tx: &mut dyn Executor,
    app_id: &str,
    lifecycle_id: &str,
) -> Result<Option<UserAppDiscoveredIdentity>, UserAppStoreError> {
    let Some(mut witness) = original(tx, app_id, lifecycle_id).await? else {
        return Ok(None);
    };
    let mut after = String::new();
    while !witness.dev_volumes.is_empty() || !witness.prod_volumes.is_empty() {
        let fields = models::Operation::fields();
        // Only this application's completed storage-removal operations enter memory, one bounded
        // page at a time. Do not use only the latest purge: it may remove a newer
        // PVC while the original discovery witness remains immutable history.
        let rows = models::Operation::filter(
            fields
                .app_id()
                .eq(app_id)
                .and(fields.lifecycle_id().eq(lifecycle_id))
                .and(
                    fields
                        .kind()
                        .eq(codec::kind_name(UserAppOperationKind::PurgeResources))
                        .or(fields
                            .kind()
                            .eq(codec::kind_name(UserAppOperationKind::DestroyDevStorage)))
                        .or(fields
                            .kind()
                            .eq(codec::kind_name(UserAppOperationKind::DestroyProdStorage)))
                        .or(fields
                            .kind()
                            .eq(codec::kind_name(UserAppOperationKind::ClearProdStorage))),
                )
                .and(
                    fields
                        .state()
                        .eq(codec::state_name(UserAppOperationState::Succeeded)),
                )
                .and(fields.operation_id().gt(after.as_str())),
        )
        .order_by(fields.operation_id().asc())
        .limit(RETIREMENT_PAGE_SIZE)
        .exec(tx)
        .await
        .map_err(storage)?;
        let exhausted = rows.len() < RETIREMENT_PAGE_SIZE;
        for row in rows {
            after = row.operation_id.clone();
            let id = row.operation_id.clone();
            let result = codec::operation(row)
                .map_err(|error| error.to_string())
                .and_then(|operation| subtract_confirmed_retirement(&mut witness, &operation));
            if let Err(error) = result {
                // Invalid historical evidence grants no retirement. Preserve
                // the raw row and continue to other independently valid receipts.
                tracing::warn!(%app_id, operation_id = %id, %error,
                    "Recovery witness retained PVCs without usable storage-removal evidence");
            }
        }
        if exhausted {
            break;
        }
    }
    Ok(Some(witness))
}
