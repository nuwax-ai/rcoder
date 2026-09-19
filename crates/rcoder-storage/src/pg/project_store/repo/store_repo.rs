//! Complete transaction operations for project registrations and retirement.
//! Callers hold the writer's canonical advisory locks before any row access.
use crate::{db::models::Project, pg::project_store::persist_ops::ProjectSnapshot};
use anyhow::{Result, ensure};
use shared_types::persistence::PersistenceOperationOutcome;
use toasty::Executor;
use toasty_core::schema::db::Type;

pub(super) fn outcome(rows: u64) -> PersistenceOperationOutcome {
    if rows == 0 {
        PersistenceOperationOutcome::Superseded
    } else {
        PersistenceOperationOutcome::Committed
    }
}

pub(in crate::pg) async fn upsert_project(
    tx: &mut dyn Executor,
    p: &ProjectSnapshot,
) -> Result<PersistenceOperationOutcome> {
    ensure!(
        p.expected_revision >= 0 && !p.generation.is_empty(),
        "Invalid project write identity"
    );
    ensure!(
        p.predecessor.as_deref() != Some(p.generation.as_str()),
        "Project cannot replace itself"
    );
    ensure!(
        p.container_name.is_some() == p.container_generation.is_some(),
        "Incomplete container reference"
    );
    let revision = p
        .expected_revision
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("Project revision exhausted"))?;
    let retired = toasty::sql::query(
        "SELECT project_id FROM project_tombstones WHERE project_id=$1 AND generation=$2",
    )
    .bind(&p.project_id)
    .bind(&p.generation)
    .exec(tx)
    .await?;
    if !retired.is_empty() {
        return Ok(PersistenceOperationOutcome::Superseded);
    }
    if let (Some(name), Some(generation)) = (&p.container_name, &p.container_generation) {
        let exists = toasty::sql::query("SELECT container_name FROM containers WHERE container_name=$1 AND container_generation=$2")
            .bind(name).bind(generation).exec(tx).await?;
        if exists.is_empty() {
            return Ok(PersistenceOperationOutcome::Superseded);
        }
    }
    if let Some(current) = Project::filter_by_project_id(&p.project_id)
        .first()
        .exec(tx)
        .await?
    {
        if current.generation == p.generation {
            if current.row_revision != p.expected_revision {
                return Ok(PersistenceOperationOutcome::Superseded);
            }
        } else {
            if p.expected_revision != 0
                || p.predecessor.as_deref() != Some(current.generation.as_str())
            {
                return Ok(PersistenceOperationOutcome::Superseded);
            }
            remove_project(tx, &p.project_id, &current.generation).await?;
        }
    } else if p.expected_revision != 0 {
        return Ok(PersistenceOperationOutcome::Superseded);
    }
    let changed = toasty::sql::statement(
        "INSERT INTO projects(project_id,generation,user_id,pod_id,tenant_id,space_id,isolation_type,container_name,container_generation,latest_session,model_provider_json,request_id,agent_status_json,service_type,payload_version,last_activity_at_us,created_at_us,row_revision)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,NULL,$10,$11,$12,$13,1,$14,$15,$16)
         ON CONFLICT(project_id) DO UPDATE SET user_id=EXCLUDED.user_id,pod_id=EXCLUDED.pod_id,tenant_id=EXCLUDED.tenant_id,space_id=EXCLUDED.space_id,isolation_type=EXCLUDED.isolation_type,
           container_name=EXCLUDED.container_name,container_generation=EXCLUDED.container_generation,latest_session=NULL,
           model_provider_json=EXCLUDED.model_provider_json,request_id=EXCLUDED.request_id,agent_status_json=EXCLUDED.agent_status_json,service_type=EXCLUDED.service_type,
           last_activity_at_us=GREATEST(projects.last_activity_at_us,EXCLUDED.last_activity_at_us),row_revision=EXCLUDED.row_revision
         WHERE projects.generation=EXCLUDED.generation AND projects.row_revision=$17")
        .bind(&p.project_id).bind(&p.generation)
        .bind_typed(p.user_id.as_deref(), Type::Text).bind_typed(p.pod_id.as_deref(), Type::Text)
        .bind_typed(p.tenant_id.as_deref(), Type::Text).bind_typed(p.space_id.as_deref(), Type::Text)
        .bind_typed(p.isolation_type.as_deref(), Type::Text).bind_typed(p.container_name.as_deref(), Type::Text)
        .bind_typed(p.container_generation.as_deref(), Type::Text)
        .bind_typed(p.model_provider.as_ref().map(serde_json::to_string).transpose()?, Type::Text)
        .bind_typed(p.request_id.as_deref(), Type::Text)
        .bind_typed(p.agent_status.as_ref().map(serde_json::to_string).transpose()?, Type::Text)
        .bind_typed(p.service_type.as_deref(), Type::Text)
        .bind(p.last_activity.timestamp_micros()).bind(p.created_at.timestamp_micros()).bind(revision).bind(p.expected_revision)
        .exec(tx).await?;
    if changed == 0 {
        return Ok(PersistenceOperationOutcome::Superseded);
    }
    // Membership and the selected pointer become visible together. Tombstones
    // prevent a stale whole-project snapshot from resurrecting removed sessions.
    for (id, generation) in &p.sessions {
        let added = super::add_session(
            tx,
            &p.project_id,
            id,
            &p.generation,
            generation,
            p.retired_sessions.get(id).map(String::as_str),
        )
        .await?;
        if added == PersistenceOperationOutcome::Superseded {
            return Ok(PersistenceOperationOutcome::Superseded);
        }
    }
    if let Some(id) = &p.latest_session {
        let generation = p
            .sessions
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("Latest session has no captured identity"))?;
        toasty::sql::statement("UPDATE projects SET latest_session=$1 WHERE project_id=$2 AND generation=$3 AND row_revision=$4 AND EXISTS(SELECT 1 FROM sessions WHERE session_id=$1 AND generation=$5 AND project_id=$2 AND project_generation=$3)")
            .bind(id).bind(&p.project_id).bind(&p.generation).bind(revision).bind(generation).exec(tx).await?;
    }
    Ok(PersistenceOperationOutcome::Committed)
}

pub(in crate::pg) async fn remove_project(
    tx: &mut dyn Executor,
    id: &str,
    generation: &str,
) -> Result<PersistenceOperationOutcome> {
    let now = chrono::Utc::now().timestamp_micros();
    toasty::sql::statement("INSERT INTO project_tombstones(project_id,generation,retired_at_us) VALUES($1,$2,$3) ON CONFLICT DO NOTHING")
        .bind(id).bind(generation).bind(now).exec(tx).await?;
    toasty::sql::statement("INSERT INTO session_tombstones(session_id,generation,retired_at_us) SELECT session_id,generation,$3 FROM sessions WHERE project_id=$1 AND project_generation=$2 ON CONFLICT DO NOTHING")
        .bind(id).bind(generation).bind(now).exec(tx).await?;
    toasty::sql::statement("DELETE FROM sessions WHERE project_id=$1 AND project_generation=$2")
        .bind(id)
        .bind(generation)
        .exec(tx)
        .await?;
    Ok(outcome(
        toasty::sql::statement("DELETE FROM projects WHERE project_id=$1 AND generation=$2")
            .bind(id)
            .bind(generation)
            .exec(tx)
            .await?,
    ))
}

pub(in crate::pg) async fn remove_project_for_container(
    tx: &mut dyn Executor,
    id: &str,
    generation: &str,
    uid: &str,
    container_name: &str,
    container_generation: &str,
) -> Result<PersistenceOperationOutcome> {
    let matches = toasty::sql::query("SELECT p.project_id FROM projects p JOIN containers c ON c.container_name=p.container_name AND c.container_generation=p.container_generation WHERE p.project_id=$1 AND p.generation=$2 AND c.container_id=$3 AND c.container_name=$4 AND c.container_generation=$5")
        .bind(id).bind(generation).bind(uid).bind(container_name).bind(container_generation).exec(tx).await?;
    if matches.is_empty() {
        return Ok(PersistenceOperationOutcome::Superseded);
    }
    remove_project(tx, id, generation).await
}

pub(in crate::pg) async fn delete_container_with_projects(
    tx: &mut dyn Executor,
    uid: &str,
    containers: &[(String, String)],
    projects: &[(String, String)],
) -> Result<PersistenceOperationOutcome> {
    let mut applied = false;
    for (name, container_generation) in containers {
        for (id, generation) in projects {
            applied |=
                remove_project_for_container(tx, id, generation, uid, name, container_generation)
                    .await?
                    == PersistenceOperationOutcome::Committed;
        }
        // Never discover deletion targets at execution time: a delayed request
        // may otherwise retire a newly registered generation with the same UID.
        let retired = toasty::sql::statement("INSERT INTO container_tombstones(container_name,container_generation,physical_uid,retired_at_us) SELECT container_name,container_generation,container_id,$4 FROM containers WHERE container_id=$1 AND container_name=$2 AND container_generation=$3 ON CONFLICT DO NOTHING")
            .bind(uid).bind(name).bind(container_generation).bind(chrono::Utc::now().timestamp_micros()).exec(tx).await?;
        let deleted = toasty::sql::statement("DELETE FROM containers c WHERE c.container_id=$1 AND c.container_name=$2 AND c.container_generation=$3 AND NOT EXISTS(SELECT 1 FROM projects p WHERE p.container_name=c.container_name AND p.container_generation=c.container_generation)")
            .bind(uid).bind(name).bind(container_generation).exec(tx).await?;
        applied |= retired > 0 || deleted > 0;
    }
    Ok(if applied {
        PersistenceOperationOutcome::Committed
    } else {
        PersistenceOperationOutcome::Superseded
    })
}
