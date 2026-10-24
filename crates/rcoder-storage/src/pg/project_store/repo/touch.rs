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

/// Called with project + container-name advisory locks in the writer's
/// transaction. Check the complete captured registration BEFORE any clock.
pub(in crate::pg) async fn apply_agent_status_observation(
    tx: &mut dyn Executor,
    id: &str,
    observation: &shared_types::AgentStatusObservation,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<PersistenceOperationOutcome> {
    let (Some(container_generation), Some(container_revision)) = (
        observation.container_generation.as_ref(),
        observation.container_revision,
    ) else {
        return Ok(PersistenceOperationOutcome::Superseded);
    };
    let status = if observation.active {
        shared_types::AgentStatus::Active
    } else {
        shared_types::AgentStatus::Idle
    };
    let applied = outcome(toasty::sql::statement(
        "UPDATE projects AS p SET agent_status_json=$1, last_activity_at_us=CASE WHEN $2 THEN GREATEST(p.last_activity_at_us,$3) ELSE p.last_activity_at_us END WHERE p.project_id=$4 AND p.generation=$5 AND p.row_revision=$6 AND p.container_name=$7 AND p.container_generation=$8 AND EXISTS (SELECT 1 FROM containers c WHERE c.container_name=p.container_name AND c.container_generation=p.container_generation AND c.row_revision=$9 AND c.container_id=$10 AND c.workload_uid IS NOT DISTINCT FROM $11)"
    ).bind(serde_json::to_string(&status)?).bind(observation.active).bind(at.timestamp_micros())
      .bind(id).bind(&observation.project_generation).bind(observation.project_revision)
      .bind(&observation.container_name).bind(container_generation).bind(container_revision)
      .bind(&observation.container_id).bind(observation.workload_uid.clone()).exec(tx).await?);
    if applied == PersistenceOperationOutcome::Committed && observation.active {
        touch_container(tx, &observation.container_name, container_generation, at).await?;
    }
    Ok(applied)
}
