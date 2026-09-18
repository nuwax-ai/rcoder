//! 全部 UserAppLifecycleStore 方法的 Turso 实现（从 sql.rs 宏逐方法移植；
//! SQL 为同一 SQLite 方言——Turso 追踪 SQLite 3.50.4 兼容）。
//!
//! 每个函数接收 worker 持有的连接；事务体保持与 sqlx 版相同的
//! 原子性/CAS/身份校验语义（domain 层完全复用）。

use turso::Connection;

use super::exec::{self as ex, integer, text};
use crate::userapp_lifecycle::{domain, storage};
use shared_types::{
    UserAppAdmissionOutcome, UserAppLifecycleRecord, UserAppOperationRecord,
    UserAppStoreError as Error,
};

const LOCKED: &str = "SELECT record FROM userapp_lifecycles WHERE app_id=?1";
const OP_BY_ID: &str = "SELECT record FROM userapp_operations WHERE app_id=?1 AND operation_id=?2";

/// Option<String> 参数：None → SQL NULL（request_id 可空列语义）。
fn opt_text(value: &Option<String>) -> turso::Value {
    match value {
        Some(s) => turso::Value::Text(s.clone()),
        None => turso::Value::Null,
    }
}

fn decode<T: serde::de::DeserializeOwned>(encoded: Option<String>) -> Result<T, Error> {
    serde_json::from_str(&encoded.ok_or(Error::NotFound)?).map_err(storage)
}

// ── 查询组 ────────────────────────────────────────────────────────────────

pub(super) async fn get_resource_binding(
    conn: &Connection,
    service_type: &shared_types::ServiceType,
    physical_uid: &str,
) -> Result<Option<shared_types::UserAppResourceBinding>, Error> {
    let record = ex::q_opt_string(
        conn,
        "SELECT record FROM userapp_resource_bindings WHERE service_type=?1 AND physical_uid=?2",
        vec![text(service_type.to_string()), text(physical_uid)],
    )
    .await?;
    record
        .map(|record| serde_json::from_str(&record).map_err(storage))
        .transpose()
}

pub(super) async fn get_application(
    conn: &Connection,
    app_id: &str,
) -> Result<Option<UserAppLifecycleRecord>, Error> {
    let encoded = ex::q_opt_string(conn, LOCKED, vec![text(app_id)]).await?;
    encoded
        .map(|value| serde_json::from_str(&value).map_err(storage))
        .transpose()
}

pub(super) async fn list_applications(
    conn: &Connection,
    after_app_id: Option<&str>,
    limit: u32,
) -> Result<Vec<UserAppLifecycleRecord>, Error> {
    if limit == 0 {
        return Err(Error::InvalidOperation(
            "page limit must be positive".into(),
        ));
    }
    let rows = ex::q_all_string(
        conn,
        "SELECT record FROM userapp_lifecycles WHERE app_id > ?1 ORDER BY app_id LIMIT ?2",
        vec![text(after_app_id.unwrap_or("")), integer(i64::from(limit))],
    )
    .await?;
    rows.iter()
        .map(|row| serde_json::from_str(row).map_err(storage))
        .collect()
}

pub(super) async fn list_control_snapshots(
    conn: &Connection,
    after_app_id: Option<&str>,
    limit: u32,
) -> Result<Vec<shared_types::UserAppControlSnapshot>, Error> {
    if limit == 0 {
        return Err(Error::InvalidOperation(
            "page limit must be positive".into(),
        ));
    }
    // 单语句快照（LEFT JOIN 同一 SELECT——application 与各 scope 操作来自
    // 同一语句视图；分页之间允许并发变化，恢复动作经 CAS 重新校验）。
    let rows = ex::q_rows(
        conn,
        "SELECT l.record, d.record, p.record, a.record FROM userapp_lifecycles l \
         LEFT JOIN userapp_operations d ON d.operation_id=json_extract(l.record, '$.active_operations.dev') AND d.app_id=l.app_id \
         LEFT JOIN userapp_operations p ON p.operation_id=json_extract(l.record, '$.active_operations.prod') AND p.app_id=l.app_id \
         LEFT JOIN userapp_operations a ON a.operation_id=json_extract(l.record, '$.active_operations.application') AND a.app_id=l.app_id \
         WHERE l.app_id > ?1 ORDER BY l.app_id LIMIT ?2",
        vec![text(after_app_id.unwrap_or("")), integer(i64::from(limit))],
    )
    .await?;
    rows.into_iter()
        .map(|row| {
            if row.len() != 4 {
                return Err(Error::InvalidOperation(
                    "control snapshot query column count mismatch".into(),
                ));
            }
            let application: UserAppLifecycleRecord =
                serde_json::from_str(ex::as_text(&row[0])?).map_err(storage)?;
            let mut operations = shared_types::UserAppActiveOperationRecords::default();
            for (encoded, scope) in [
                (row[1].clone(), shared_types::UserAppOperationScope::Dev),
                (row[2].clone(), shared_types::UserAppOperationScope::Prod),
                (
                    row[3].clone(),
                    shared_types::UserAppOperationScope::Application,
                ),
            ] {
                let operation: Option<UserAppOperationRecord> = match encoded {
                    turso::Value::Text(s) => Some(serde_json::from_str(&s).map_err(storage)?),
                    turso::Value::Null => None,
                    other => {
                        return Err(Error::InvalidOperation(format!(
                            "expected text/null column, got {other:?}"
                        )));
                    }
                };
                match (
                    application.active_operations.slot(scope),
                    operation.as_ref(),
                ) {
                    (None, None) => {}
                    (Some(id), Some(operation))
                        if operation.operation_id == *id
                            && operation.app_id == application.app_id
                            && operation.lifecycle_id == application.lifecycle_id
                            && !operation.state.is_terminal() => {}
                    _ => {
                        return Err(Error::InvalidOperation(
                            "Lifecycle active-operation linkage is inconsistent".into(),
                        ));
                    }
                }
                operations.set(scope, operation);
            }
            Ok(shared_types::UserAppControlSnapshot {
                application,
                operations,
            })
        })
        .collect()
}

pub(super) async fn get_operation(
    conn: &Connection,
    app_id: &str,
    operation_id: &str,
) -> Result<Option<UserAppOperationRecord>, Error> {
    let encoded = ex::q_opt_string(conn, OP_BY_ID, vec![text(app_id), text(operation_id)]).await?;
    encoded
        .map(|value| serde_json::from_str(&value).map_err(storage))
        .transpose()
}

pub(super) async fn get_operation_by_request(
    conn: &Connection,
    app_id: &str,
    request_id: &str,
) -> Result<Option<UserAppOperationRecord>, Error> {
    domain::validate_request_id(request_id)?;
    let encoded = ex::q_opt_string(
        conn,
        "SELECT o.record FROM userapp_operation_requests r JOIN userapp_operations o \
         ON o.app_id=r.app_id AND o.operation_id=r.operation_id \
         WHERE r.app_id=?1 AND r.request_id=?2",
        vec![text(app_id), text(request_id)],
    )
    .await?;
    encoded
        .map(|record| serde_json::from_str(&record).map_err(storage))
        .transpose()
}

pub(super) async fn unfinished_operations(
    conn: &Connection,
    after_operation_id: Option<&str>,
    limit: u32,
) -> Result<Vec<UserAppOperationRecord>, Error> {
    if limit == 0 {
        return Err(Error::InvalidOperation(
            "scan limit must be positive".into(),
        ));
    }
    let rows = ex::q_all_string(
        conn,
        "SELECT record FROM userapp_operations WHERE terminal=0 AND operation_id > ?1 \
         ORDER BY operation_id LIMIT ?2",
        vec![
            text(after_operation_id.unwrap_or("")),
            integer(i64::from(limit)),
        ],
    )
    .await?;
    rows.iter()
        .map(|row| serde_json::from_str(row).map_err(storage))
        .collect()
}

pub(super) async fn operation_deadline(
    conn: &Connection,
    app_id: &str,
    operation_id: &str,
) -> Result<Option<i64>, Error> {
    ex::q_opt_i64(
        conn,
        "SELECT deadline_ms FROM userapp_operation_deadlines WHERE app_id=?1 AND operation_id=?2",
        vec![text(app_id), text(operation_id)],
    )
    .await
}

pub(super) async fn get_operation_lease(
    conn: &Connection,
    app_id: &str,
    operation_id: &str,
) -> Result<Option<shared_types::UserAppOperationLeaseBinding>, Error> {
    let encoded = ex::q_opt_string(
        conn,
        "SELECT record FROM userapp_operation_leases WHERE app_id=?1 AND operation_id=?2",
        vec![text(app_id), text(operation_id)],
    )
    .await?;
    encoded
        .map(|value| serde_json::from_str(&value).map_err(storage))
        .transpose()
}

pub(super) async fn terminal_operation_leases(
    conn: &Connection,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<shared_types::UserAppOperationLeaseBinding>, Error> {
    let rows = ex::q_rows(
        conn,
        "SELECT l.record AS lease_record,o.record AS operation_record \
         FROM userapp_operation_leases l JOIN userapp_operations o ON l.operation_id=o.operation_id \
         WHERE o.terminal=1 AND l.operation_id > ?1 ORDER BY l.operation_id LIMIT ?2",
        vec![text(after.unwrap_or("")), integer(i64::from(limit.clamp(1, 1000)))],
    )
    .await?;
    let mut bindings = Vec::new();
    for row in rows {
        if row.len() != 2 {
            return Err(Error::InvalidOperation(
                "terminal lease query column count mismatch".into(),
            ));
        }
        let operation: UserAppOperationRecord =
            serde_json::from_str(ex::as_text(&row[1])?).map_err(storage)?;
        if operation.state.is_terminal() {
            bindings.push(serde_json::from_str(ex::as_text(&row[0])?).map_err(storage)?);
        }
    }
    Ok(bindings)
}

// ── 事务组 ────────────────────────────────────────────────────────────────

pub(super) async fn ensure_identity(
    conn: &mut Connection,
    app_id: &str,
) -> Result<UserAppLifecycleRecord, Error> {
    let proposed = domain::identity(app_id)?;
    let mut tx = ex::begin(conn).await?;
    tx.exec(
        "INSERT INTO userapp_lifecycles(app_id,record) VALUES(?1,?2) ON CONFLICT(app_id) DO NOTHING",
        vec![text(app_id), text(serde_json::to_string(&proposed).map_err(storage)?)],
    )
    .await?;
    let encoded = tx.one_string(LOCKED, vec![text(app_id)]).await?;
    let app: UserAppLifecycleRecord = serde_json::from_str(&encoded).map_err(storage)?;
    domain::validate_active(&app)?;
    tx.commit().await?;
    Ok(app)
}

pub(super) async fn import_application(
    conn: &mut Connection,
    legacy: &shared_types::AppMetadataRecord,
) -> Result<UserAppLifecycleRecord, Error> {
    let mut proposed = domain::identity(&legacy.app_id)?;
    proposed.name = legacy.name.clone();
    proposed.tenant_id = legacy.tenant_id.clone();
    proposed.space_id = legacy.space_id.clone();
    proposed.created_at = legacy.created_at;
    let mut tx = ex::begin(conn).await?;
    tx.exec(
        "INSERT INTO userapp_lifecycles(app_id,record) VALUES(?1,?2) ON CONFLICT(app_id) DO NOTHING",
        vec![
            text(&legacy.app_id),
            text(serde_json::to_string(&proposed).map_err(storage)?),
        ],
    )
    .await?;
    let encoded = tx.one_string(LOCKED, vec![text(&legacy.app_id)]).await?;
    let app: UserAppLifecycleRecord = serde_json::from_str(&encoded).map_err(storage)?;
    tx.commit().await?;
    Ok(app)
}

pub(super) async fn patch_metadata(
    conn: &mut Connection,
    patch: &shared_types::UserAppMetadataPatch,
) -> Result<UserAppLifecycleRecord, Error> {
    let mut tx = ex::begin(conn).await?;
    let encoded = tx.opt_string(LOCKED, vec![text(&patch.app_id)]).await?;
    let mut app: UserAppLifecycleRecord = decode(encoded)?;
    let previous = app.clone();
    domain::patch_metadata(&mut app, patch)?;
    if previous != app {
        let updated = tx
            .exec(
                "UPDATE userapp_lifecycles SET record=?2 WHERE app_id=?1",
                vec![
                    text(&app.app_id),
                    text(serde_json::to_string(&app).map_err(storage)?),
                ],
            )
            .await?;
        if updated != 1 {
            return Err(Error::VersionConflict);
        }
    }
    tx.commit().await?;
    Ok(app)
}

pub(super) async fn commit_resource_binding(
    conn: &mut Connection,
    binding: &shared_types::UserAppResourceBinding,
    progress: &shared_types::UserAppOperationProgress,
) -> Result<UserAppOperationRecord, Error> {
    if progress.app_id != binding.app_id
        || progress.lifecycle_id != binding.lifecycle_id
        || progress.state != shared_types::UserAppOperationState::Succeeded
        || binding.adopted_by_operation != progress.operation_id
    {
        return Err(Error::InvalidOperation(
            "Binding requires its successful adoption operation".into(),
        ));
    }
    let mut tx = ex::begin(conn).await?;
    let encoded = tx.opt_string(LOCKED, vec![text(&binding.app_id)]).await?;
    let mut app: UserAppLifecycleRecord = decode(encoded)?;
    domain::validate_active(&app)?;
    if app.lifecycle_id != binding.lifecycle_id {
        return Err(Error::LifecycleConflict);
    }
    let encoded = tx
        .opt_string(
            OP_BY_ID,
            vec![text(&binding.app_id), text(&progress.operation_id)],
        )
        .await?;
    let mut operation: UserAppOperationRecord = decode(encoded)?;
    if operation.kind != shared_types::UserAppOperationKind::AdoptBuilder {
        return Err(Error::InvalidOperation(
            "Binding requires explicit adoption admission".into(),
        ));
    }
    let context = shared_types::UserAppExecutionContext {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        operation_id: operation.operation_id.clone(),
        executor_id: progress.executor_id.clone(),
        request_fingerprint: operation.request_fingerprint.clone(),
    };
    binding
        .validate(&context, &binding.physical_uid)
        .map_err(Error::InvalidOperation)?;
    domain::advance(&mut app, &mut operation, progress)?;
    tx.exec(
        "INSERT INTO userapp_resource_bindings(service_type,physical_uid,app_id,record) \
         VALUES(?1,?2,?3,?4) ON CONFLICT(service_type,physical_uid) DO NOTHING",
        vec![
            text(binding.service_type.to_string()),
            text(&binding.physical_uid),
            text(&binding.app_id),
            text(serde_json::to_string(binding).map_err(storage)?),
        ],
    )
    .await?;
    let encoded = tx
        .one_string(
            "SELECT record FROM userapp_resource_bindings WHERE service_type=?1 AND physical_uid=?2",
            vec![text(binding.service_type.to_string()), text(&binding.physical_uid)],
        )
        .await?;
    let previous: shared_types::UserAppResourceBinding =
        serde_json::from_str(&encoded).map_err(storage)?;
    previous
        .validate(&context, &binding.physical_uid)
        .map_err(|_| Error::LifecycleConflict)?;
    let updated = tx
        .exec(
            "UPDATE userapp_operations SET record=?2,terminal=1 WHERE operation_id=?1",
            vec![
                text(&operation.operation_id),
                text(serde_json::to_string(&operation).map_err(storage)?),
            ],
        )
        .await?;
    if updated != 1 {
        return Err(Error::VersionConflict);
    }
    let updated = tx
        .exec(
            "UPDATE userapp_lifecycles SET record=?2 WHERE app_id=?1",
            vec![
                text(&app.app_id),
                text(serde_json::to_string(&app).map_err(storage)?),
            ],
        )
        .await?;
    if updated != 1 {
        return Err(Error::VersionConflict);
    }
    tx.exec(
        "DELETE FROM userapp_operation_inputs WHERE operation_id=?1 AND app_id=?2 AND lifecycle_id=?3",
        vec![
            text(&operation.operation_id),
            text(&operation.app_id),
            text(&operation.lifecycle_id),
        ],
    )
    .await?;
    tx.commit().await?;
    Ok(operation)
}

pub(super) async fn admit_with_input(
    conn: &mut Connection,
    request: &shared_types::UserAppAdmission,
    input: Option<&shared_types::UserAppExecutionInput>,
) -> Result<UserAppAdmissionOutcome, Error> {
    match (&request.command, input) {
        (None, Some(input))
            if request.kind == shared_types::UserAppOperationKind::AdoptBuilder
                && request.request_fingerprint == input.digest() => {}
        (None, _) if request.kind == shared_types::UserAppOperationKind::AdoptBuilder => {
            return Err(Error::InvalidOperation(
                "Adoption requires its original input digest".into(),
            ));
        }
        (
            Some(
                shared_types::UserAppControlCommand::Create { input_digest }
                | shared_types::UserAppControlCommand::Update { input_digest }
                | shared_types::UserAppControlCommand::Deploy { input_digest, .. },
            ),
            Some(input),
        ) if *input_digest == input.digest() => {}
        (
            Some(
                shared_types::UserAppControlCommand::Create { .. }
                | shared_types::UserAppControlCommand::Update { .. }
                | shared_types::UserAppControlCommand::Deploy { .. },
            ),
            _,
        )
        | (_, Some(_)) => {
            return Err(Error::InvalidOperation(
                "Private execution input does not match command digest".into(),
            ));
        }
        (_, None) => {}
    }
    let proposed = domain::identity(&request.app_id)?;
    let mut tx = ex::begin(conn).await?;
    tx.exec(
        "INSERT INTO userapp_lifecycles(app_id,record) VALUES(?1,?2) ON CONFLICT(app_id) DO NOTHING",
        vec![
            text(&request.app_id),
            text(serde_json::to_string(&proposed).map_err(storage)?),
        ],
    )
    .await?;
    let encoded = tx.one_string(LOCKED, vec![text(&request.app_id)]).await?;
    let mut app: UserAppLifecycleRecord = serde_json::from_str(&encoded).map_err(storage)?;
    if let Some(request_id) = &request.request_id {
        let recreation = tx
            .opt_string(
                "SELECT request_id FROM userapp_recreations WHERE app_id=?1 AND request_id=?2",
                vec![text(&request.app_id), text(request_id)],
            )
            .await?;
        if recreation.is_some() {
            return Err(Error::InvalidOperation(
                "request identity was already used for lifecycle recreation".into(),
            ));
        }
    }
    let duplicates = tx
        .all_string(
            "SELECT DISTINCT o.record FROM userapp_operations o \
             LEFT JOIN userapp_operation_requests r ON r.operation_id=o.operation_id AND r.app_id=o.app_id \
             WHERE o.app_id=?1 AND (o.operation_id=?2 OR r.request_id=?3)",
            vec![
                text(&request.app_id),
                text(&request.operation_id),
                opt_text(&request.request_id),
            ],
        )
        .await?;
    if duplicates.len() > 1 {
        return Err(Error::InvalidOperation(
            "request and operation identities refer to different operations".into(),
        ));
    }
    let duplicate = duplicates
        .first()
        .map(|value| serde_json::from_str(value).map_err(storage))
        .transpose()?;
    let mut active = shared_types::UserAppActiveOperationRecords::default();
    for scope in shared_types::UserAppOperationScope::ALL {
        let Some(slot_id) = app.active_operations.slot(scope).cloned() else {
            continue;
        };
        let encoded = tx
            .opt_string(OP_BY_ID, vec![text(&request.app_id), text(&slot_id)])
            .await?;
        let record = encoded
            .ok_or_else(|| Error::InvalidOperation("active operation record is missing".into()))?;
        active.set(scope, Some(serde_json::from_str(&record).map_err(storage)?));
    }
    let result = domain::admission(&mut app, request, duplicate, &active)?;
    if let UserAppAdmissionOutcome::Accepted(operation) = &result {
        tx.exec(
            "INSERT INTO userapp_operations(operation_id,app_id,request_id,terminal,record) VALUES(?1,?2,?3,0,?4)",
            vec![
                text(&operation.operation_id),
                text(&operation.app_id),
                opt_text(&operation.request_id),
                text(serde_json::to_string(operation).map_err(storage)?),
            ],
        )
        .await?;
        if let Some(input) = input {
            tx.exec(
                "INSERT INTO userapp_operation_inputs(operation_id,app_id,lifecycle_id,payload) VALUES(?1,?2,?3,?4)",
                vec![
                    text(&operation.operation_id),
                    text(&operation.app_id),
                    text(&operation.lifecycle_id),
                    text(input.encoded()),
                ],
            )
            .await?;
        }
        let updated = tx
            .exec(
                "UPDATE userapp_lifecycles SET record=?2 WHERE app_id=?1",
                vec![
                    text(&app.app_id),
                    text(serde_json::to_string(&app).map_err(storage)?),
                ],
            )
            .await?;
        if updated != 1 {
            return Err(Error::VersionConflict);
        }
    }
    // Joining is also an accepted idempotent request. Register its alias in the
    // same transaction as admission. Keep the original caller token.
    if let Some(request_id) = &request.request_id {
        let operation = match &result {
            UserAppAdmissionOutcome::Accepted(op) | UserAppAdmissionOutcome::Existing(op) => op,
        };
        let inserted = tx
            .exec(
                "INSERT INTO userapp_operation_requests(app_id,request_id,operation_id) VALUES(?1,?2,?3) \
                 ON CONFLICT(app_id,request_id) DO NOTHING",
                vec![
                    text(&request.app_id),
                    text(request_id),
                    text(&operation.operation_id),
                ],
            )
            .await?;
        if inserted == 0 {
            let owner = tx
                .one_string(
                    "SELECT operation_id FROM userapp_operation_requests WHERE app_id=?1 AND request_id=?2",
                    vec![text(&request.app_id), text(request_id)],
                )
                .await?;
            if owner != operation.operation_id {
                return Err(Error::VersionConflict);
            }
        }
    }
    tx.commit().await?;
    Ok(result)
}

pub(super) async fn read_execution_input(
    conn: &mut Connection,
    context: &shared_types::UserAppExecutionContext,
) -> Result<shared_types::UserAppExecutionInput, Error> {
    context
        .validate_identity(&context.app_id)
        .map_err(Error::InvalidOperation)?;
    let mut tx = ex::begin(conn).await?;
    let encoded = tx.opt_string(LOCKED, vec![text(&context.app_id)]).await?;
    let app: UserAppLifecycleRecord = decode(encoded)?;
    if app.lifecycle_id != context.lifecycle_id
        || !domain::operation_owns_any_slot(&app, &context.operation_id)
    {
        return Err(Error::LifecycleConflict);
    }
    let encoded = tx
        .opt_string(
            OP_BY_ID,
            vec![text(&context.app_id), text(&context.operation_id)],
        )
        .await?;
    let operation: UserAppOperationRecord = decode(encoded)?;
    if !domain::slot_matches_operation(&app, &operation) {
        return Err(Error::LifecycleConflict);
    }
    if operation.executor_id.as_deref() != Some(context.executor_id.as_str())
        || operation.lifecycle_id != context.lifecycle_id
        || operation.request_fingerprint != context.request_fingerprint
        || operation.state != shared_types::UserAppOperationState::Running
    {
        return Err(Error::VersionConflict);
    }
    let payload = tx
        .opt_string(
            "SELECT payload FROM userapp_operation_inputs WHERE app_id=?1 AND operation_id=?2 AND lifecycle_id=?3",
            vec![
                text(&context.app_id),
                text(&context.operation_id),
                text(&context.lifecycle_id),
            ],
        )
        .await?;
    let input = shared_types::UserAppExecutionInput::new(payload.ok_or(Error::NotFound)?);
    match &operation.command {
        None if operation.kind == shared_types::UserAppOperationKind::AdoptBuilder
            && operation.request_fingerprint == input.digest() => {}
        Some(
            shared_types::UserAppControlCommand::Create { input_digest }
            | shared_types::UserAppControlCommand::Update { input_digest }
            | shared_types::UserAppControlCommand::Deploy { input_digest, .. },
        ) if *input_digest == input.digest() => {}
        _ => {
            return Err(Error::InvalidOperation(
                "Stored execution input digest mismatch".into(),
            ));
        }
    }
    tx.commit().await?;
    Ok(input)
}

pub(super) async fn bind_operation_deadline(
    conn: &mut Connection,
    app_id: &str,
    operation_id: &str,
    lifecycle_id: &str,
    deadline_epoch_ms: i64,
) -> Result<i64, Error> {
    let mut tx = ex::begin(conn).await?;
    let encoded = tx.opt_string(LOCKED, vec![text(app_id)]).await?;
    let app: UserAppLifecycleRecord = decode(encoded)?;
    if app.lifecycle_id != lifecycle_id || !domain::operation_owns_any_slot(&app, operation_id) {
        return Err(Error::LifecycleConflict);
    }
    let encoded = tx
        .opt_string(OP_BY_ID, vec![text(app_id), text(operation_id)])
        .await?;
    let operation: UserAppOperationRecord = decode(encoded)?;
    if !domain::slot_matches_operation(&app, &operation) {
        return Err(Error::LifecycleConflict);
    }
    if operation.lifecycle_id != lifecycle_id || operation.state.is_terminal() {
        return Err(Error::VersionConflict);
    }
    // bind-once：INSERT ON CONFLICT DO NOTHING；已存在 → 返回持久值
    tx.exec(
        "INSERT INTO userapp_operation_deadlines(operation_id,app_id,lifecycle_id,deadline_ms) VALUES(?1,?2,?3,?4) \
         ON CONFLICT(operation_id) DO NOTHING",
        vec![
            text(operation_id),
            text(app_id),
            text(lifecycle_id),
            integer(deadline_epoch_ms),
        ],
    )
    .await?;
    // deadline_ms 整数列经 get_value 返回 Integer——读回字符串列再解析或直接
    // 读值都行；这里走 q_rows 单行单列以统一解包。
    let rows = ex::q_rows_tx(
        &mut tx,
        "SELECT deadline_ms FROM userapp_operation_deadlines WHERE operation_id=?1 AND app_id=?2 AND lifecycle_id=?3",
        vec![text(operation_id), text(app_id), text(lifecycle_id)],
    )
    .await?;
    let persisted = match rows.first().map(|row| row.first()) {
        Some(Some(turso::Value::Integer(i))) => *i,
        other => {
            return Err(Error::InvalidOperation(format!(
                "deadline readback corrupted: {other:?}"
            )));
        }
    };
    tx.commit().await?;
    Ok(persisted)
}

pub(super) async fn bind_operation_lease(
    conn: &mut Connection,
    context: &shared_types::UserAppExecutionContext,
    receipt: &shared_types::UserAppOperationLeaseReceipt,
) -> Result<(), Error> {
    context
        .validate_identity(&context.app_id)
        .map_err(Error::InvalidOperation)?;
    receipt.validate().map_err(Error::InvalidOperation)?;
    let mut tx = ex::begin(conn).await?;
    let encoded = tx.opt_string(LOCKED, vec![text(&context.app_id)]).await?;
    let app: UserAppLifecycleRecord = decode(encoded)?;
    if app.lifecycle_id != context.lifecycle_id
        || !domain::operation_owns_any_slot(&app, &context.operation_id)
    {
        return Err(Error::LifecycleConflict);
    }
    let encoded = tx
        .opt_string(
            OP_BY_ID,
            vec![text(&context.app_id), text(&context.operation_id)],
        )
        .await?;
    let operation: UserAppOperationRecord = decode(encoded)?;
    if !domain::slot_matches_operation(&app, &operation) {
        return Err(Error::LifecycleConflict);
    }
    let builder = operation.scope == shared_types::UserAppOperationScope::Dev;
    if operation.state != shared_types::UserAppOperationState::Running
        || operation.executor_id.as_deref() != Some(context.executor_id.as_str())
        || operation.lifecycle_id != context.lifecycle_id
        || operation.request_fingerprint != context.request_fingerprint
        || builder != (*receipt.service_type() == shared_types::ServiceType::UserappBuilder)
    {
        return Err(Error::VersionConflict);
    }
    let binding = shared_types::UserAppOperationLeaseBinding {
        context: context.clone(),
        receipt: receipt.clone(),
    };
    let encoded = serde_json::to_string(&binding).map_err(storage)?;
    let inserted = tx
        .exec(
            "INSERT INTO userapp_operation_leases(operation_id,app_id,lifecycle_id,record) VALUES(?1,?2,?3,?4) \
             ON CONFLICT(operation_id) DO NOTHING",
            vec![
                text(&context.operation_id),
                text(&context.app_id),
                text(&context.lifecycle_id),
                text(&encoded),
            ],
        )
        .await?;
    if inserted == 0 {
        let current = tx
            .one_string(
                "SELECT record FROM userapp_operation_leases WHERE operation_id=?1 AND app_id=?2 AND lifecycle_id=?3",
                vec![
                    text(&context.operation_id),
                    text(&context.app_id),
                    text(&context.lifecycle_id),
                ],
            )
            .await?;
        let current: shared_types::UserAppOperationLeaseBinding =
            serde_json::from_str(&current).map_err(storage)?;
        if current != binding {
            return Err(Error::VersionConflict);
        }
    }
    tx.commit().await?;
    Ok(())
}

pub(super) async fn forget_operation_lease(
    conn: &mut Connection,
    binding: &shared_types::UserAppOperationLeaseBinding,
) -> Result<(), Error> {
    let mut tx = ex::begin(conn).await?;
    // Lock the application row even for an old terminal lifecycle.
    let _locked = tx
        .opt_string(LOCKED, vec![text(&binding.context.app_id)])
        .await?;
    let encoded = tx
        .opt_string(
            OP_BY_ID,
            vec![
                text(&binding.context.app_id),
                text(&binding.context.operation_id),
            ],
        )
        .await?;
    let operation: UserAppOperationRecord = decode(encoded)?;
    if !operation.state.is_terminal()
        || operation.lifecycle_id != binding.context.lifecycle_id
        || operation.executor_id.as_deref() != Some(binding.context.executor_id.as_str())
        || operation.request_fingerprint != binding.context.request_fingerprint
    {
        return Err(Error::VersionConflict);
    }
    let encoded = tx
        .opt_string(
            "SELECT record FROM userapp_operation_leases WHERE app_id=?1 AND operation_id=?2",
            vec![
                text(&binding.context.app_id),
                text(&binding.context.operation_id),
            ],
        )
        .await?;
    if let Some(encoded) = encoded {
        let stored: shared_types::UserAppOperationLeaseBinding =
            serde_json::from_str(&encoded).map_err(storage)?;
        if &stored != binding {
            return Err(Error::VersionConflict);
        }
        let deleted = tx
            .exec(
                "DELETE FROM userapp_operation_leases WHERE operation_id=?1 AND app_id=?2",
                vec![
                    text(&binding.context.operation_id),
                    text(&binding.context.app_id),
                ],
            )
            .await?;
        if deleted != 1 {
            return Err(Error::VersionConflict);
        }
    }
    tx.commit().await?;
    Ok(())
}

pub(super) async fn reserve_completed_operation(
    conn: &mut Connection,
    snapshot: &UserAppOperationRecord,
) -> Result<UserAppOperationRecord, Error> {
    let mut tx = ex::begin(conn).await?;
    let encoded = tx.opt_string(LOCKED, vec![text(&snapshot.app_id)]).await?;
    let app: UserAppLifecycleRecord = decode(encoded)?;
    if app.lifecycle_id != snapshot.lifecycle_id
        || !domain::operation_owns_any_slot(&app, &snapshot.operation_id)
    {
        return Err(Error::LifecycleConflict);
    }
    let encoded = tx
        .opt_string(
            OP_BY_ID,
            vec![text(&snapshot.app_id), text(&snapshot.operation_id)],
        )
        .await?;
    let mut operation: UserAppOperationRecord = decode(encoded)?;
    if !domain::slot_matches_operation(&app, &operation) {
        return Err(Error::LifecycleConflict);
    }
    if &operation != snapshot
        || !matches!(
            operation.state,
            shared_types::UserAppOperationState::Running
                | shared_types::UserAppOperationState::RecoveryRequired
        )
        || !shared_types::userapp_operation_has_final_evidence(&operation)
    {
        return Err(Error::VersionConflict);
    }
    if operation.kind == shared_types::UserAppOperationKind::EnsureBuilder {
        let _evidence: shared_types::BuilderCreationEvidence =
            serde_json::from_value(operation.checkpoint.clone()).map_err(storage)?;
        if app.state != shared_types::UserAppLifecycleState::Active {
            return Err(Error::LifecycleConflict);
        }
    } else {
        let binding = tx
            .opt_string(
                "SELECT record FROM userapp_operation_leases WHERE app_id=?1 AND operation_id=?2 AND lifecycle_id=?3",
                vec![
                    text(&operation.app_id),
                    text(&operation.operation_id),
                    text(&operation.lifecycle_id),
                ],
            )
            .await?;
        let binding: shared_types::UserAppOperationLeaseBinding = decode(binding)?;
        if binding.context.app_id != operation.app_id
            || binding.context.operation_id != operation.operation_id
            || binding.context.lifecycle_id != operation.lifecycle_id
            || &binding.context.executor_id
                != operation
                    .executor_id
                    .as_ref()
                    .ok_or(Error::VersionConflict)?
            || binding.context.request_fingerprint != operation.request_fingerprint
        {
            return Err(Error::VersionConflict);
        }
        binding
            .receipt
            .validate()
            .map_err(Error::InvalidOperation)?;
    }
    operation.revision = operation
        .revision
        .checked_add(1)
        .ok_or_else(|| Error::InvalidOperation("Operation revision exhausted".into()))?;
    operation.state = shared_types::UserAppOperationState::Running;
    let updated = tx
        .exec(
            "UPDATE userapp_operations SET record=?2 WHERE operation_id=?1 AND app_id=?3",
            vec![
                text(&operation.operation_id),
                text(serde_json::to_string(&operation).map_err(storage)?),
                text(&operation.app_id),
            ],
        )
        .await?;
    if updated != 1 {
        return Err(Error::VersionConflict);
    }
    tx.commit().await?;
    Ok(operation)
}

pub(super) async fn advance(
    conn: &mut Connection,
    progress: &shared_types::UserAppOperationProgress,
) -> Result<UserAppOperationRecord, Error> {
    let mut tx = ex::begin(conn).await?;
    let encoded = tx.opt_string(LOCKED, vec![text(&progress.app_id)]).await?;
    let mut app: UserAppLifecycleRecord = decode(encoded)?;
    let encoded = tx
        .opt_string(
            OP_BY_ID,
            vec![text(&progress.app_id), text(&progress.operation_id)],
        )
        .await?;
    let mut operation: UserAppOperationRecord = decode(encoded)?;
    domain::advance(&mut app, &mut operation, progress)?;
    let updated = tx
        .exec(
            "UPDATE userapp_operations SET record=?2,terminal=?3 WHERE operation_id=?1",
            vec![
                text(&operation.operation_id),
                text(serde_json::to_string(&operation).map_err(storage)?),
                integer(i64::from(operation.state.is_terminal())),
            ],
        )
        .await?;
    if updated != 1 {
        return Err(Error::VersionConflict);
    }
    let updated = tx
        .exec(
            "UPDATE userapp_lifecycles SET record=?2 WHERE app_id=?1",
            vec![
                text(&app.app_id),
                text(serde_json::to_string(&app).map_err(storage)?),
            ],
        )
        .await?;
    if updated != 1 {
        return Err(Error::VersionConflict);
    }
    if operation.state.is_terminal() {
        tx.exec(
            "DELETE FROM userapp_operation_inputs WHERE operation_id=?1 AND app_id=?2 AND lifecycle_id=?3",
            vec![
                text(&operation.operation_id),
                text(&operation.app_id),
                text(&operation.lifecycle_id),
            ],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(operation)
}

pub(super) async fn recreate(
    conn: &mut Connection,
    app_id: &str,
    expected_lifecycle_id: &str,
    request_id: &str,
) -> Result<UserAppLifecycleRecord, Error> {
    domain::validate_request_id(request_id)?;
    let mut tx = ex::begin(conn).await?;
    let encoded = tx.opt_string(LOCKED, vec![text(app_id)]).await?;
    let mut app: UserAppLifecycleRecord = decode(encoded)?;
    let control = tx
        .opt_string(
            "SELECT request_id FROM userapp_operation_requests WHERE app_id=?1 AND request_id=?2",
            vec![text(app_id), text(request_id)],
        )
        .await?;
    if control.is_some() {
        return Err(Error::InvalidOperation(
            "request identity was already used for a control operation".into(),
        ));
    }
    let previous = ex::q_rows_tx(
        &mut tx,
        "SELECT previous_lifecycle_id,new_lifecycle_id FROM userapp_recreations WHERE app_id=?1 AND request_id=?2",
        vec![text(app_id), text(request_id)],
    )
    .await?;
    if let Some(row) = previous.first() {
        let old = ex::as_text(row.first().ok_or(Error::NotFound)?)?;
        let new = ex::as_text(row.get(1).ok_or(Error::NotFound)?)?;
        if old != expected_lifecycle_id || new != app.lifecycle_id {
            return Err(Error::LifecycleConflict);
        }
        tx.commit().await?;
        return Ok(app);
    }
    if app.lifecycle_id != expected_lifecycle_id
        || app.state != shared_types::UserAppLifecycleState::Deleted
        || !app.active_operations.is_empty()
    {
        return Err(Error::LifecycleConflict);
    }
    app.lifecycle_id = uuid::Uuid::new_v4().to_string();
    app.lifecycle_epoch = app
        .lifecycle_epoch
        .checked_add(1)
        .ok_or_else(|| Error::InvalidOperation("lifecycle epoch exhausted".into()))?;
    app.metadata_revision = 1;
    app.state = shared_types::UserAppLifecycleState::Active;
    app.created_at = chrono::Utc::now();
    app.name = None;
    app.tenant_id = None;
    app.space_id = None;
    app.runtime_policy = shared_types::UserAppRuntimePolicy::default();
    tx.exec(
        "INSERT INTO userapp_recreations(app_id,request_id,previous_lifecycle_id,new_lifecycle_id) VALUES(?1,?2,?3,?4)",
        vec![
            text(app_id),
            text(request_id),
            text(expected_lifecycle_id),
            text(&app.lifecycle_id),
        ],
    )
    .await?;
    let updated = tx
        .exec(
            "UPDATE userapp_lifecycles SET record=?2 WHERE app_id=?1",
            vec![
                text(app_id),
                text(serde_json::to_string(&app).map_err(storage)?),
            ],
        )
        .await?;
    if updated != 1 {
        return Err(Error::VersionConflict);
    }
    tx.commit().await?;
    Ok(app)
}
