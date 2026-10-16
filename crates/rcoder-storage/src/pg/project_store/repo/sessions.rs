//! Session ownership is immutable within a generation. Callers execute these
//! statements in the same transaction as the project snapshot and hold the
//! project/session advisory locks in the writer's canonical order.
use anyhow::Result;
use shared_types::persistence::PersistenceOperationOutcome;
use toasty::Executor;
use toasty_core::schema::db::Type;

use super::store_repo::outcome;

pub(in crate::pg) async fn append_session_for_container(
    tx: &mut dyn Executor,
    append: &crate::pg::project_store::persist_ops::ContainerSessionAppend,
) -> Result<PersistenceOperationOutcome> {
    let Some(container) =
        crate::db::models::Container::filter_by_container_name(&append.container_name)
            .first()
            .exec(tx)
            .await?
    else {
        return Ok(PersistenceOperationOutcome::Superseded);
    };
    if !container
        .service_type
        .parse::<shared_types::ServiceType>()
        .map_err(anyhow::Error::msg)?
        .same_container_family(
            append
                .service_type
                .parse::<shared_types::ServiceType>()
                .map_err(anyhow::Error::msg)?,
        )
    {
        return Ok(PersistenceOperationOutcome::Superseded);
    }
    let owned = toasty::sql::query(
        "SELECT p.project_id FROM projects p JOIN containers c
           ON c.container_name=p.container_name AND c.container_generation=p.container_generation
         WHERE p.project_id=$1 AND p.generation=$2 AND c.container_name=$3
           AND c.container_generation=$4 AND c.container_id=$5
           AND c.workload_uid IS NOT DISTINCT FROM $6 AND c.logical_id=$7
           AND NOT EXISTS(SELECT 1 FROM project_tombstones t WHERE t.project_id=p.project_id AND t.generation=p.generation)
           AND NOT EXISTS(SELECT 1 FROM container_tombstones t WHERE
             (t.container_name=c.container_name AND t.container_generation=c.container_generation)
             OR t.physical_uid=c.container_id)"
    ).bind(&append.project_id).bind(&append.project_generation)
        .bind(&append.container_name).bind(&append.container_generation).bind(&append.physical_uid)
        .bind_typed(append.workload_uid.as_deref(), Type::Text)
        .bind(&append.logical_id).exec(tx).await?;
    if owned.is_empty() {
        return Ok(PersistenceOperationOutcome::Superseded);
    }
    let outcome = add_session(
        tx,
        &append.project_id,
        &append.session_id,
        &append.project_generation,
        &append.session_generation,
        append.predecessor.as_deref(),
    )
    .await?;
    if outcome == PersistenceOperationOutcome::Committed {
        super::touch_project(
            tx,
            &append.project_id,
            &append.project_generation,
            append.at,
        )
        .await?;
        super::touch_container(
            tx,
            &append.container_name,
            &append.container_generation,
            append.at,
        )
        .await?;
        toasty::sql::statement(
            "UPDATE projects SET latest_session=$1 WHERE project_id=$2 AND generation=$3",
        )
        .bind(&append.session_id)
        .bind(&append.project_id)
        .bind(&append.project_generation)
        .exec(tx)
        .await?;
    }
    Ok(outcome)
}

pub(in crate::pg) async fn add_session(
    tx: &mut dyn Executor,
    project_id: &str,
    session_id: &str,
    project_generation: &str,
    generation: &str,
    predecessor: Option<&str>,
) -> Result<PersistenceOperationOutcome> {
    anyhow::ensure!(
        predecessor != Some(generation),
        "Session cannot replace its own generation"
    );
    let now = chrono::Utc::now().timestamp_micros();
    // A replay may refresh activity, but cannot move an existing generation to
    // another project. Replacement requires the explicitly captured predecessor.
    let changed = toasty::sql::statement(
        "INSERT INTO sessions(session_id,generation,project_id,project_generation,created_at_us,last_seen_at_us)
         SELECT $1,$2,$3,$4,$5,$5
         WHERE EXISTS(SELECT 1 FROM projects WHERE project_id=$3 AND generation=$4)
           AND NOT EXISTS(SELECT 1 FROM session_tombstones WHERE session_id=$1 AND generation=$2)
         ON CONFLICT(session_id) DO UPDATE SET generation=EXCLUDED.generation,
           project_id=EXCLUDED.project_id,project_generation=EXCLUDED.project_generation,
           created_at_us=CASE WHEN sessions.generation=EXCLUDED.generation THEN sessions.created_at_us ELSE EXCLUDED.created_at_us END,
           last_seen_at_us=GREATEST(sessions.last_seen_at_us,EXCLUDED.last_seen_at_us)
         WHERE (sessions.generation=EXCLUDED.generation AND sessions.project_id=EXCLUDED.project_id AND sessions.project_generation=EXCLUDED.project_generation)
            OR (sessions.generation=$6 AND sessions.project_id=EXCLUDED.project_id AND sessions.project_generation=EXCLUDED.project_generation)"
    ).bind(session_id).bind(generation).bind(project_id).bind(project_generation)
        .bind(now).bind_typed(predecessor, Type::Text).exec(tx).await?;
    if changed > 0
        && let Some(previous) = predecessor
    {
        toasty::sql::statement("INSERT INTO session_tombstones(session_id,generation,retired_at_us) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
                .bind(session_id).bind(previous).bind(now).exec(tx).await?;
    }
    Ok(outcome(changed))
}

pub(in crate::pg) async fn remove_session(
    tx: &mut dyn Executor,
    session_id: &str,
    generation: &str,
) -> Result<PersistenceOperationOutcome> {
    // Write the tombstone even when an earlier queued insert has not arrived.
    // Clear latest_session only for the exact owner being removed, never for a
    // newer incarnation reusing the session ID.
    toasty::sql::statement("INSERT INTO session_tombstones(session_id,generation,retired_at_us) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(session_id).bind(generation).bind(chrono::Utc::now().timestamp_micros()).exec(tx).await?;
    toasty::sql::statement("UPDATE projects p SET latest_session=NULL WHERE p.latest_session=$1 AND EXISTS(SELECT 1 FROM sessions s WHERE s.session_id=$1 AND s.generation=$2 AND s.project_id=p.project_id AND s.project_generation=p.generation)")
        .bind(session_id).bind(generation).exec(tx).await?;
    let changed =
        toasty::sql::statement("DELETE FROM sessions WHERE session_id=$1 AND generation=$2")
            .bind(session_id)
            .bind(generation)
            .exec(tx)
            .await?;
    Ok(outcome(changed))
}

pub(in crate::pg) async fn clear_sessions(
    tx: &mut dyn Executor,
    project_id: &str,
    project_generation: &str,
    sessions: &[(String, String)],
) -> Result<PersistenceOperationOutcome> {
    let mut changed = false;
    for (id, generation) in sessions {
        // Unlike removing one globally unique generation, bulk cleanup must
        // prove the captured project's ownership for each individual session.
        let different_owner = toasty::sql::query("SELECT session_id FROM sessions WHERE session_id=$1 AND generation=$2 AND (project_id<>$3 OR project_generation<>$4)")
            .bind(id).bind(generation).bind(project_id).bind(project_generation).exec(tx).await?;
        if different_owner.is_empty() {
            changed |=
                remove_session(tx, id, generation).await? == PersistenceOperationOutcome::Committed;
        }
    }
    Ok(if changed {
        PersistenceOperationOutcome::Committed
    } else {
        PersistenceOperationOutcome::Superseded
    })
}
