//! Checked projection of normalized Toasty rows into the public preview contract.
use crate::db::models::PreviewInstance;
use shared_types::{PreviewInstanceRecord, PreviewInstanceState, PreviewStoreError as Error};

pub(super) fn unavailable(error: impl std::fmt::Display) -> Error {
    Error::Unavailable(error.to_string())
}
pub(super) fn parse_state(raw: &str) -> Result<PreviewInstanceState, Error> {
    use PreviewInstanceState::*;
    match raw {
        "starting" => Ok(Starting),
        "ready" => Ok(Ready),
        "stopping" => Ok(Stopping),
        "stopped" => Ok(Stopped),
        "failed" => Ok(Failed),
        "unknown" => Ok(Unknown),
        _ => Err(unavailable("Unknown persisted preview state")),
    }
}
pub(super) fn state_str(state: PreviewInstanceState) -> &'static str {
    use PreviewInstanceState::*;
    match state {
        Starting => "starting",
        Ready => "ready",
        Stopping => "stopping",
        Stopped => "stopped",
        Failed => "failed",
        Unknown => "unknown",
    }
}
fn timestamp(value: i64) -> Result<chrono::DateTime<chrono::Utc>, Error> {
    chrono::DateTime::from_timestamp_micros(value)
        .ok_or_else(|| unavailable("Invalid persisted preview timestamp"))
}
pub(super) fn record(row: PreviewInstance) -> Result<PreviewInstanceRecord, Error> {
    let state = parse_state(&row.state)?;
    let port = row
        .port
        .map(|value| u16::try_from(value).map_err(unavailable))
        .transpose()?;
    if row.revision < 1
        || row.instance_id.is_empty()
        || row.host_id.is_empty()
        || row.pid.is_some_and(|pid| pid <= 0)
        || port == Some(0)
        || (state.is_active() && port.is_none())
    {
        return Err(unavailable("Invalid persisted preview identity or port"));
    }
    let operation_id = row
        .operation_id
        .filter(|id| !id.is_empty())
        .ok_or_else(|| unavailable("Preview operation pointer is missing"))?;
    Ok(PreviewInstanceRecord {
        preview_key: row.preview_key,
        project_id: row.project_id,
        project_path: row.project_path,
        instance_id: row.instance_id,
        revision: row.revision,
        operation_id,
        host_id: row.host_id,
        pod_name: row.pod_name,
        pod_ip: row.pod_ip,
        pid: row.pid,
        port,
        base_path: row.base_path,
        state,
        last_heartbeat_at: row.last_heartbeat_at_us.map(timestamp).transpose()?,
        last_activity_at: timestamp(row.last_activity_at_us)?,
        detail: row.detail,
        updated_at: timestamp(row.updated_at_us)?,
    })
}
