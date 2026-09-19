//! 重启隔离（Turso 版——同业务规则：不确定转 RecoveryRequired，
//! 不清租约、不重放写）。从 mod.rs 拆出（2026-09-19 文件拆分）。

use super::exec::{begin, text};
use crate::userapp_lifecycle::storage;
use shared_types::{
    UserAppOperationRecord, UserAppOperationState as State, UserAppStoreError as Error,
};

pub(super) async fn quarantine(conn: &mut turso::Connection) -> Result<(), Error> {
    let mut tx = begin(conn).await?;
    let records = tx
        .all_string(
            "SELECT record FROM userapp_operations WHERE terminal=0 \
             AND json_extract(record, '$.state') IN ('Running','WaitingRetry')",
            vec![],
        )
        .await?;
    let count = records.len();
    for original in records {
        let mut record: UserAppOperationRecord =
            serde_json::from_str(&original).map_err(storage)?;
        record.state = State::RecoveryRequired;
        record.revision = record.revision.checked_add(1).ok_or_else(|| {
            Error::InvalidOperation("Operation revision exhausted during restart".into())
        })?;
        record.error_code = Some(shared_types::error_codes::ERR_BACKEND_ERROR.into());
        record.error_message =
            Some("Previous local executor stopped; remote outcome requires verification".into());
        let encoded = serde_json::to_string(&record).map_err(storage)?;
        let updated = tx
            .exec(
                "UPDATE userapp_operations SET record=?1 WHERE operation_id=?2 AND record=?3 AND terminal=0",
                vec![text(encoded), text(&record.operation_id), text(original)],
            )
            .await?;
        if updated != 1 {
            return Err(Error::VersionConflict);
        }
    }
    tx.commit().await?;
    if count != 0 {
        tracing::warn!(
            operations = count,
            "Interrupted local operations require remote outcome verification"
        );
    }
    Ok(())
}
