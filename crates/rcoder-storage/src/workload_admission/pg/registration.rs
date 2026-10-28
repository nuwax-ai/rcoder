//! Minimal canonical projections: never hydrate unrelated model/status JSON.
use super::super::*;
use tokio_postgres::{Client, GenericClient, IsolationLevel};

pub(super) async fn has_references(client: &Client, key: &WorkloadKey) -> Result<bool> {
    let row = client.query_one("SELECT EXISTS(SELECT 1 FROM containers WHERE container_name=$1 UNION ALL SELECT 1 FROM projects WHERE container_name=$1)", &[&key.logical_name()]).await?;
    Ok(row.try_get(0)?)
}
pub(super) async fn capture(
    client: &mut Client,
    key: &WorkloadKey,
    owner: Arc<()>,
) -> Result<WorkloadRegistrationFrame> {
    let tx = client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await?;
    let frame = read(&tx, key, owner).await?;
    if let Some(container) = &frame.container {
        let family: ServiceType = container.family.parse().map_err(anyhow::Error::msg)?;
        ensure!(
            family.same_container_family(key.family()),
            "Canonical container belongs to another family"
        );
    }
    tx.commit().await?;
    Ok(frame)
}
async fn read(
    db: &impl GenericClient,
    key: &WorkloadKey,
    owner: Arc<()>,
) -> Result<WorkloadRegistrationFrame> {
    let name = key.logical_name();
    let container = db.query_opt("SELECT service_type,container_generation,row_revision,container_id,workload_uid FROM containers WHERE container_name=$1", &[&name]).await?
        .map(|row| Ok::<_, anyhow::Error>(ContainerRegistration { family: row.try_get(0)?, generation: row.try_get(1)?, revision: row.try_get(2)?, physical_uid: row.try_get(3)?, workload_uid: row.try_get(4)? })).transpose()?;
    let projects = db.query("SELECT project_id,generation,row_revision,container_generation,user_id FROM projects WHERE container_name=$1 ORDER BY project_id", &[&name]).await?.iter()
        .map(|row| Ok::<_, anyhow::Error>(ProjectRegistration { id: row.try_get(0)?, generation: row.try_get(1)?, revision: row.try_get(2)?, container_generation: row.try_get(3)?, user_id: row.try_get(4)? })).collect::<Result<Vec<_>>>()?;
    let sessions = db.query("SELECT s.session_id,s.generation,s.project_id,s.project_generation FROM sessions s JOIN projects p ON p.project_id=s.project_id AND p.generation=s.project_generation WHERE p.container_name=$1 ORDER BY s.session_id", &[&name]).await?.iter()
        .map(|row| Ok::<_, anyhow::Error>(SessionRegistration { id: row.try_get(0)?, generation: row.try_get(1)?, project_id: row.try_get(2)?, project_generation: row.try_get(3)? })).collect::<Result<Vec<_>>>()?;
    Ok(WorkloadRegistrationFrame {
        key: key.clone(),
        owner,
        container_name: name.into(),
        container,
        projects,
        sessions,
        memory: Vec::new(),
    })
}
pub(super) async fn retire(
    client: &mut Client,
    key: &WorkloadKey,
    frame: &WorkloadRegistrationFrame,
) -> Result<PersistenceWriteOutcome> {
    let tx = client.transaction().await?;
    let mut locks =
        std::collections::BTreeSet::from([format!("container-name:{}", frame.container_name)]);
    if let Some(uid) = frame.physical_uid() {
        locks.insert(format!("container:{uid}"));
    }
    locks.extend(
        frame
            .projects
            .iter()
            .map(|row| format!("project:{}", row.id)),
    );
    locks.extend(
        frame
            .sessions
            .iter()
            .map(|row| format!("session:{}", row.id)),
    );
    for lock in locks {
        tx.query_one(
            "SELECT 1 FROM pg_advisory_xact_lock(hashtextextended($1,719324))",
            &[&lock],
        )
        .await?;
    }
    let current = read(&tx, key, frame.owner.clone()).await?;
    if current.container != frame.container
        || current.projects != frame.projects
        || current.sessions != frame.sessions
    {
        tx.rollback().await?;
        return Ok(PersistenceWriteOutcome::Superseded);
    }
    let now = chrono::Utc::now().timestamp_micros();
    for project in &frame.projects {
        tx.execute("INSERT INTO project_tombstones(project_id,generation,retired_at_us) VALUES($1,$2,$3) ON CONFLICT DO NOTHING", &[&project.id,&project.generation,&now]).await?;
        tx.execute("INSERT INTO session_tombstones(session_id,generation,retired_at_us) SELECT session_id,generation,$3 FROM sessions WHERE project_id=$1 AND project_generation=$2 ON CONFLICT DO NOTHING", &[&project.id,&project.generation,&now]).await?;
        tx.execute(
            "DELETE FROM sessions WHERE project_id=$1 AND project_generation=$2",
            &[&project.id, &project.generation],
        )
        .await?;
        let changed = tx.execute("DELETE FROM projects WHERE project_id=$1 AND generation=$2 AND row_revision=$3 AND container_name=$4 AND container_generation IS NOT DISTINCT FROM $5", &[&project.id,&project.generation,&project.revision,&frame.container_name,&project.container_generation]).await?;
        if changed != 1 {
            tx.rollback().await?;
            return Ok(PersistenceWriteOutcome::Superseded);
        }
    }
    if let Some(container) = &frame.container {
        tx.execute("INSERT INTO container_tombstones(container_name,container_generation,physical_uid,retired_at_us) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING", &[&frame.container_name,&container.generation,&container.physical_uid,&now]).await?;
        let changed = tx.execute("DELETE FROM containers c WHERE container_name=$1 AND container_generation=$2 AND row_revision=$3 AND container_id IS NOT DISTINCT FROM $4 AND workload_uid IS NOT DISTINCT FROM $5 AND NOT EXISTS(SELECT 1 FROM projects p WHERE p.container_name=c.container_name)", &[&frame.container_name,&container.generation,&container.revision,&container.physical_uid,&container.workload_uid]).await?;
        if changed != 1 {
            tx.rollback().await?;
            return Ok(PersistenceWriteOutcome::Superseded);
        }
    }
    // A COMMIT error is unknown. The caller returns Deferred and retires this
    // whole session; it never replays the write as an alleged rollback.
    tx.commit().await?;
    Ok(PersistenceWriteOutcome::Committed)
}
