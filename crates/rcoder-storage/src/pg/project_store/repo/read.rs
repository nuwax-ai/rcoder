//! Every multi-table caller holds one repeatable-read transaction. Generation
//! predicates, rather than matching names alone, authorize route hydration.
use super::rows::{ContainerRow, ProjectRow, SessionRow};
use crate::db::models::{Container, Project, Session};
use anyhow::{Context, Result, ensure};
use toasty::Executor;

pub(in crate::pg) async fn fetch_container_registration(
    tx: &mut dyn Executor,
    name: &str,
) -> Result<Option<ContainerRow>> {
    Container::filter_by_container_name(name)
        .first()
        .exec(tx)
        .await?
        .map(TryInto::try_into)
        .transpose()
}

pub(in crate::pg) async fn fetch_all_containers(
    tx: &mut dyn Executor,
) -> Result<Vec<ContainerRow>> {
    Container::all()
        .exec(tx)
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect()
}
pub(in crate::pg) async fn fetch_all_sessions(tx: &mut dyn Executor) -> Result<Vec<SessionRow>> {
    Session::all()
        .exec(tx)
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect()
}
pub(in crate::pg) async fn fetch_all_projects_with_sessions(
    tx: &mut dyn Executor,
    sessions: Vec<SessionRow>,
) -> Result<Vec<ProjectRow>> {
    let mut groups = std::collections::HashMap::<(String, String), Vec<SessionRow>>::new();
    for row in sessions {
        groups
            .entry((row.project_id.clone(), row.project_generation.clone()))
            .or_default()
            .push(row);
    }
    let mut result = Vec::new();
    for row in Project::all().exec(tx).await? {
        let sessions = groups
            .remove(&(row.project_id.clone(), row.generation.clone()))
            .unwrap_or_default();
        result.push(ProjectRow::from_model(row, sessions)?);
    }
    ensure!(
        groups.is_empty(),
        "Persistent sessions reference missing project generations"
    );
    Ok(result)
}

pub(in crate::pg) async fn fetch_project_by_session(
    tx: &mut dyn Executor,
    session_id: &str,
) -> Result<Option<(ProjectRow, Option<ContainerRow>)>> {
    if shared_types::SessionHint::classify(Some(session_id))
        .as_session_id()
        .is_none()
    {
        return Ok(None);
    }
    let Some(session) = Session::filter_by_session_id(session_id)
        .first()
        .exec(tx)
        .await?
    else {
        return Ok(None);
    };
    let session = SessionRow::try_from(session)?;
    let project = Project::filter_by_project_id(&session.project_id)
        .first()
        .exec(tx)
        .await?
        .context("Session project is missing")?;
    ensure!(
        project.generation == session.project_generation,
        "Session belongs to a different project generation"
    );
    read_project_registration(tx, project).await.map(Some)
}

pub(in crate::pg) async fn fetch_project_registration(
    tx: &mut dyn Executor,
    project_id: &str,
) -> Result<Option<(ProjectRow, Option<ContainerRow>)>> {
    let Some(project) = Project::filter_by_project_id(project_id)
        .first()
        .exec(tx)
        .await?
    else {
        return Ok(None);
    };
    read_project_registration(tx, project).await.map(Some)
}

async fn read_project_registration(
    tx: &mut dyn Executor,
    project: Project,
) -> Result<(ProjectRow, Option<ContainerRow>)> {
    let fields = Session::fields();
    let sessions = Session::filter(
        fields
            .project_id()
            .eq(&project.project_id)
            .and(fields.project_generation().eq(&project.generation)),
    )
    .exec(tx)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect::<Result<Vec<_>>>()?;
    let project = ProjectRow::from_model(project, sessions)?;
    let container = match (&project.container_name, &project.container_generation) {
        (Some(name), Some(generation)) => {
            let row = Container::filter_by_container_name(name)
                .first()
                .exec(tx)
                .await?
                .context("Project container registration is missing")?;
            ensure!(
                &row.container_generation == generation,
                "Project cannot route to a replacement container generation"
            );
            Some(ContainerRow::try_from(row)?)
        }
        (None, None) => None,
        _ => anyhow::bail!("Incomplete project container identity"),
    };
    Ok((project, container))
}

pub(in crate::pg) async fn project_exists(tx: &mut dyn Executor, id: &str) -> Result<bool> {
    Ok(Project::filter_by_project_id(id)
        .first()
        .exec(tx)
        .await?
        .is_some())
}

pub(in crate::pg) async fn project_generation_is_retired(
    tx: &mut dyn Executor,
    project_id: &str,
    generation: &str,
) -> Result<bool> {
    Ok(!toasty::sql::query(
        "SELECT project_id FROM project_tombstones WHERE project_id=$1 AND generation=$2",
    )
    .bind(project_id)
    .bind(generation)
    .exec(tx)
    .await?
    .is_empty())
}

pub(in crate::pg) async fn container_generation_is_retired(
    tx: &mut dyn Executor,
    name: &str,
    generation: &str,
    physical_uid: &str,
) -> Result<bool> {
    Ok(!toasty::sql::query(
        "SELECT container_name FROM container_tombstones WHERE container_name=$1 AND container_generation=$2 AND physical_uid=$3",
    )
    .bind(name)
    .bind(generation)
    .bind(physical_uid)
    .exec(tx)
    .await?
    .is_empty())
}
pub(in crate::pg) async fn session_exists(tx: &mut dyn Executor, id: &str) -> Result<bool> {
    Ok(Session::filter_by_session_id(id)
        .first()
        .exec(tx)
        .await?
        .is_some())
}
