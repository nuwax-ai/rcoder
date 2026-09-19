//! Activity is monotonic and never changes a structural revision. Delayed
//! observations must carry their captured incarnation, not resolve by name.
use super::store_repo::outcome;
use anyhow::Result;
use shared_types::persistence::PersistenceOperationOutcome;
use toasty::Executor;

pub(in crate::pg) async fn touch_container(
    tx: &mut dyn Executor,
    name: &str,
    generation: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<PersistenceOperationOutcome> {
    Ok(outcome(toasty::sql::statement("UPDATE containers SET last_activity_at_us=$1 WHERE container_name=$2 AND container_generation=$3 AND last_activity_at_us<$1")
        .bind(at.timestamp_micros()).bind(name).bind(generation).exec(tx).await?))
}
pub(in crate::pg) async fn touch_project(
    tx: &mut dyn Executor,
    id: &str,
    generation: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<PersistenceOperationOutcome> {
    Ok(outcome(toasty::sql::statement("UPDATE projects SET last_activity_at_us=$1 WHERE project_id=$2 AND generation=$3 AND last_activity_at_us<$1")
        .bind(at.timestamp_micros()).bind(id).bind(generation).exec(tx).await?))
}
pub(in crate::pg) async fn touch_session(
    tx: &mut dyn Executor,
    id: &str,
    generation: &str,
    project_id: &str,
    project_generation: &str,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<PersistenceOperationOutcome> {
    Ok(outcome(toasty::sql::statement("UPDATE sessions SET last_seen_at_us=$1 WHERE session_id=$2 AND generation=$3 AND project_id=$4 AND project_generation=$5 AND last_seen_at_us<$1")
        .bind(at.timestamp_micros()).bind(id).bind(generation).bind(project_id).bind(project_generation).exec(tx).await?))
}
pub(in crate::pg) async fn update_agent_status(
    tx: &mut dyn Executor,
    id: &str,
    generation: &str,
    expected_revision: i64,
    status: &serde_json::Value,
) -> Result<PersistenceOperationOutcome> {
    // Status observations do not grant a new revision to an old whole-project
    // snapshot. A changed structural revision makes this observation obsolete.
    Ok(outcome(toasty::sql::statement("UPDATE projects SET agent_status_json=$1 WHERE project_id=$2 AND generation=$3 AND row_revision=$4")
        .bind(serde_json::to_string(status)?).bind(id).bind(generation).bind(expected_revision).exec(tx).await?))
}
