//! Read only the current root, slots and operation headers for startup fences.
//! No operation payload or archive participates, and no execution is admitted.
use super::*;
use toasty_core::stmt::Value;

fn text(value: &Value) -> Option<&str> {
    match value {
        Value::String(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

fn slot(value: &Value) -> Option<Option<&str>> {
    match value {
        Value::Null => Some(None),
        value => text(value).map(Some),
    }
}

async fn active_kinds(
    tx: &mut dyn Executor,
    backend: Backend,
    app_id: &str,
    lifecycle_id: &str,
    slots: [Option<&str>; 3],
) -> Result<Option<Vec<UserAppOperationKind>>, Error> {
    let mut kinds = Vec::new();
    for (scope, id) in UserAppOperationScope::ALL.into_iter().zip(slots) {
        let Some(id) = id else { continue };
        let rows = toasty::sql::query(repo::sql(backend,
            "SELECT operation_id,app_id,lifecycle_id,kind,scope,state FROM userapp_operations WHERE operation_id=$1"))
            .bind(id).exec(tx).await.map_err(storage)?;
        let [Value::Record(record)] = rows.as_slice() else {
            return Ok(None);
        };
        let [
            actual_id,
            actual_app,
            actual_lifecycle,
            kind,
            actual_scope,
            state,
        ] = record.fields.as_slice()
        else {
            return Ok(None);
        };
        let Some(kind) = text(kind).and_then(|kind| codec::kind(kind).ok()) else {
            return Ok(None);
        };
        let Some(actual_scope) = text(actual_scope).and_then(|scope| codec::scope(scope).ok())
        else {
            return Ok(None);
        };
        let Some(state) = text(state).and_then(|state| codec::state(state).ok()) else {
            return Ok(None);
        };
        if text(actual_id) != Some(id)
            || text(actual_app) != Some(app_id)
            || text(actual_lifecycle) != Some(lifecycle_id)
            || actual_scope != scope
            || kind.scope() != scope
            || state.is_terminal()
        {
            return Ok(None);
        }
        kinds.push(kind);
    }
    Ok(Some(kinds))
}

pub(crate) async fn list_wake_fence_snapshots(
    tx: &mut dyn Executor,
    backend: Backend,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<UserAppWakeFenceSnapshot>, Error> {
    if limit == 0 {
        return Err(Error::InvalidOperation(
            "page limit must be positive".into(),
        ));
    }
    // Include all raw app IDs so a clear page never hides a later deletion or
    // unknown header. Root and operation headers share this owned transaction.
    let rows = toasty::sql::query(repo::sql(backend,
        "SELECT a.app_id,a.lifecycle_id,a.lifecycle_state,s.app_id,s.lifecycle_id,s.dev_operation_id,s.prod_operation_id,s.application_operation_id FROM userapps a LEFT JOIN userapp_active_operations s ON s.app_id=a.app_id WHERE a.app_id>$1 ORDER BY a.app_id LIMIT $2"))
        .bind(after.unwrap_or("")).bind(i64::from(limit)).exec(tx).await.map_err(storage)?;
    let mut snapshots = Vec::with_capacity(rows.len());
    for row in rows {
        let Value::Record(record) = row else {
            return Err(Error::InvalidOperation(
                "Invalid wake fence identity row".into(),
            ));
        };
        let fields = record.fields.as_slice();
        let app_id = fields.first().and_then(text).ok_or_else(|| {
            Error::InvalidOperation("Wake fence application cursor is unavailable".into())
        })?;
        let mut snapshot = UserAppWakeFenceSnapshot {
            app_id: app_id.into(),
            state: UserAppWakeFenceState::Unconfirmed,
        };
        if let [_, life, state, slot_app, slot_life, dev, prod, application] = fields
            && let Some(life) = text(life)
            && let Some(state) = text(state).and_then(|state| codec::lifecycle(state).ok())
        {
            if state != UserAppLifecycleState::Active {
                snapshot.state = UserAppWakeFenceState::Blocked;
            } else if text(slot_app) == Some(app_id)
                && text(slot_life) == Some(life)
                && let (Some(dev), Some(prod), Some(application)) =
                    (slot(dev), slot(prod), slot(application))
                && !(application.is_some() && (dev.is_some() || prod.is_some()))
                && !(dev.is_some() && dev == prod)
                && let Some(kinds) =
                    active_kinds(tx, backend, app_id, life, [dev, prod, application]).await?
            {
                snapshot =
                    UserAppWakeFenceSnapshot::from_confirmed_headers(app_id.into(), state, kinds);
            }
        }
        if snapshot.state == UserAppWakeFenceState::Unconfirmed {
            tracing::warn!(
                app_id,
                "Current wake fence header unconfirmed; retain only this application's wake fence"
            );
        }
        snapshots.push(snapshot);
    }
    Ok(snapshots)
}
