//! Private, read-only configuration salvage from completed operations. No replay.
use super::{codec, repo, storage};
use crate::userapp_lifecycle::domain;
use shared_types::{UserAppControlCommand, UserAppExecutionInput, UserAppStoreError as Error};
use toasty::Executor;
use toasty_core::stmt::Value;

pub(super) async fn read(
    tx: &mut dyn Executor,
    backend: super::Backend,
    app_id: &str,
    lifecycle_id: &str,
) -> Result<Option<UserAppExecutionInput>, Error> {
    let app = repo::app(tx, app_id).await?.ok_or(Error::NotFound)?;
    domain::validate_active(&app)?;
    if app.lifecycle_id != lifecycle_id {
        return Err(Error::LifecycleConflict);
    }
    let mut offset = 0i64;
    loop {
        let rows=toasty::sql::query(repo::sql(backend,
            "SELECT i.payload,i.payload_digest,o.command_json,o.kind,o.operation_id FROM userapp_operation_inputs i JOIN userapp_operations o ON o.operation_id=i.operation_id AND o.app_id=i.app_id AND o.lifecycle_id=i.lifecycle_id WHERE o.app_id=$1 AND o.lifecycle_id=$2 AND o.scope='prod' AND o.state='succeeded' AND o.kind IN ('create','update','start_deployment','restart_deployment') AND i.payload_version=1 ORDER BY o.created_at_us DESC,o.operation_id DESC LIMIT 64 OFFSET $3"))
            .bind(app_id).bind(lifecycle_id).bind(offset).exec(&mut *tx).await.map_err(storage)?;
        let count = rows.len();
        for row in rows {
            let Value::Record(row) = row else {
                return Err(Error::InvalidOperation(
                    "Invalid retained configuration row".into(),
                ));
            };
            let [
                Value::String(payload),
                Value::String(digest),
                command,
                Value::String(kind),
                Value::String(operation_id),
            ] = row.fields.as_slice()
            else {
                return Err(Error::InvalidOperation(
                    "Invalid retained configuration fields".into(),
                ));
            };
            let Value::String(command) = command else {
                continue;
            };
            let input = UserAppExecutionInput::new(payload.clone());
            let decoded = serde_json::from_str::<UserAppControlCommand>(command);
            let valid_command = match decoded.as_ref() {
                Ok(command) if codec::kind_name(command.kind()) == kind => match command {
                    UserAppControlCommand::Create { input_digest }
                    | UserAppControlCommand::Update { input_digest }
                    | UserAppControlCommand::Deploy { input_digest, .. } => input_digest == digest,
                    UserAppControlCommand::StopBuilder
                    | UserAppControlCommand::RestartBuilder
                    | UserAppControlCommand::Start { .. }
                    | UserAppControlCommand::Restart
                    | UserAppControlCommand::PrepareProdDatabase
                    | UserAppControlCommand::Stop { .. }
                    | UserAppControlCommand::SetRecyclePolicy { .. }
                    | UserAppControlCommand::DeleteResources { .. }
                    | UserAppControlCommand::DeleteApplication
                    | UserAppControlCommand::DestroyStorage { .. }
                    | UserAppControlCommand::ResetDatabasePassword { .. }
                    | UserAppControlCommand::ClearStorage { .. } => false,
                },
                Ok(_) | Err(_) => false,
            };
            if input.digest() != *digest || !valid_command {
                tracing::warn!(
                    app_id,
                    operation_id,
                    "Completed private runtime configuration is unreadable or has invalid identity; original bytes retained"
                );
                continue;
            }
            let value = match serde_json::from_str::<serde_json::Value>(payload) {
                Ok(value) => value,
                Err(_) => {
                    tracing::warn!(
                        app_id,
                        operation_id,
                        "Completed private runtime input is malformed; original bytes retained"
                    );
                    continue;
                }
            };
            if value.get("version").and_then(serde_json::Value::as_u64) == Some(1)
                && value
                    .get("params")
                    .is_some_and(serde_json::Value::is_object)
            {
                return Ok(Some(input));
            }
        }
        if count < 64 {
            return Ok(None);
        }
        offset = offset.checked_add(64).ok_or_else(|| {
            Error::InvalidOperation("Retained runtime history is too large".into())
        })?;
    }
}
