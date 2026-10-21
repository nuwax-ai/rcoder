//! 启动全量加载 + 行→领域对象重建（sync 任务复用）
//!
//! 加载顺序 containers → projects → sessions（与 FK 一致）。
//! 重建走 `inner.insert/insert_with_session/add_session_to_project`（内存实现的
//! 唯一一份业务逻辑），天然旁路持久化（PgStore 的 enqueue 只在其包装层）。
//! session 按非 latest 优先、latest 最后的顺序回放，保证 `latest_session` 复原。

use std::collections::HashMap;
use std::sync::Arc;

use crate::db::owner::DatabaseOwner;
use anyhow::Context as _;
use tracing::info;

use shared_types::{
    AgentStatus, ContainerBasicInfo, ModelProviderConfig, ProjectAndContainerInfo, ServiceType,
    SessionHint,
};

use super::repo::{self, ContainerRow, ProjectRow, SessionRow};
use crate::adapter::ProjectAdapter;

/// 全量加载（容器数 = 历史活跃容器，量级小）
pub(crate) async fn load_all(
    owner: &DatabaseOwner,
    inner: &ProjectAdapter,
) -> anyhow::Result<HashMap<String, shared_types::persistence::ContainerPersistenceIdentity>> {
    let (containers, projects, sessions) = super::database::snapshot(owner).await?;
    let identities = containers
        .iter()
        .map(|row| (row.container_name.clone(), row.persistence_identity()))
        .collect();
    apply_snapshot(inner, container_rows_to_map(containers), projects, sessions)?;
    info!("[STORAGE_PG] boot load complete");
    Ok(identities)
}

/// containers 行 → 基本信息映射（hydrate 与 sync 共用）
pub(super) fn container_rows_to_map(
    containers: Vec<ContainerRow>,
) -> HashMap<String, ContainerRow> {
    containers
        .into_iter()
        .map(|row| (row.container_name.clone(), row))
        .collect()
}

/// 单行转换（回源直查的单容器 hydrate 复用）
pub(super) fn container_row_to_basic(row: &ContainerRow) -> ContainerBasicInfo {
    ContainerBasicInfo {
        container_id: row.container_id.clone().unwrap_or_default(),
        container_name: row.container_name.clone(),
        container_ip: row.container_ip.clone(),
        internal_port: row.internal_port,
        external_port: row.external_port,
        project_id: row.logical_id.clone(),
        status: row.status.clone(),
        created_at: row.created_at,
        service_url: row.service_url.clone(),
        workload_uid: row.workload_uid.clone(),
    }
}

/// project 行 → 领域对象；损坏字段和代次不一致均拒绝加载。
pub(super) fn hydrate_project(
    row: &ProjectRow,
    container_by_name: &HashMap<String, ContainerRow>,
) -> anyhow::Result<ProjectAndContainerInfo> {
    let service_type = parse_service_type(row.service_type.as_deref())
        .context("Persistent project service type is invalid")?;
    let container = match (&row.container_name, &row.container_generation) {
        (Some(name), Some(generation)) => {
            let container = container_by_name
                .get(name)
                .context("Persistent project container is missing")?;
            anyhow::ensure!(
                &container.container_generation == generation,
                "Project container generation mismatch"
            );
            Some(container)
        }
        (None, None) => None,
        _ => anyhow::bail!("Incomplete project container identity"),
    };
    let mut info = ProjectAndContainerInfo::new(row.project_id.clone());
    info.set_persistence_identity(shared_types::persistence::ProjectPersistenceIdentity {
        generation: row.generation.clone(),
        revision: row.row_revision,
        predecessor: None,
        sessions: row.session_identities.clone(),
        retired_sessions: Default::default(),
        container: container.map(
            |c| shared_types::persistence::ContainerPersistenceIdentity {
                generation: c.container_generation.clone(),
                revision: c.row_revision,
                physical_uid: c.container_id.clone(),
                predecessor: None,
                predecessor_revision: None,
            },
        ),
    });
    info.set_service_type(Some(service_type));
    info.set_user_id(row.user_id.clone());
    info.set_pod_id(row.pod_id.clone());
    info.set_scope(
        row.tenant_id.clone(),
        row.space_id.clone(),
        row.isolation_type.clone(),
    );
    info.set_request_id(row.request_id.clone());
    info.set_timestamps(row.created_at, row.last_activity);
    if let Some(value) = row.model_provider.clone() {
        let provider = serde_json::from_value::<ModelProviderConfig>(value)
            .map_err(|_| anyhow::anyhow!("Invalid persisted model provider configuration"))?;
        info.set_model_provider(Some(provider));
    }
    if let Some(value) = row.agent_status.clone() {
        let status = serde_json::from_value::<AgentStatus>(value)
            .map_err(|_| anyhow::anyhow!("Invalid persisted agent status"))?;
        info.set_status(Some(status));
    }
    if let Some(container) = container {
        info.set_container(Some(container_row_to_basic(container)));
    }
    for (sid, generation) in &row.session_identities {
        info.restore_session_identity(sid, generation.clone());
    }
    anyhow::ensure!(
        info.restore_latest_session(row.latest_session.as_deref()),
        "Latest session is not owned by project"
    );
    Ok(info)
}

/// 全量快照应用到镜像（启动加载与 sync 共用；latest 最后回放复原 latest_session）
pub(super) fn apply_snapshot(
    inner: &ProjectAdapter,
    container_by_name: HashMap<String, ContainerRow>,
    projects: Vec<ProjectRow>,
    sessions: Vec<SessionRow>,
) -> anyhow::Result<()> {
    let total_sessions = sessions.len();
    let loaded = projects.len();
    for row in projects {
        let info = hydrate_project(&row, &container_by_name)?;
        inner.insert(row.project_id.clone(), Arc::new(info))?;
    }
    info!(
        "[STORAGE_PG] snapshot applied: {loaded} projects, {} containers, {total_sessions} sessions",
        container_by_name.len()
    );
    Ok(())
}

fn parse_service_type(s: Option<&str>) -> Option<ServiceType> {
    use std::str::FromStr as _;
    s.and_then(|value: &str| ServiceType::from_str(value).ok())
}

// ========== SSE 回源直查（miss → PG 单查 → hydrate 镜像） ==========

#[derive(Clone, Copy)]
enum ProjectRead<'a> {
    Session(&'a str),
    Registration { retired_generation: Option<&'a str> },
}

fn same_registration_scope(
    current: &ProjectAndContainerInfo,
    persisted: &ProjectAndContainerInfo,
) -> bool {
    current.project_id() == persisted.project_id()
        && current.user_id() == persisted.user_id()
        && current.pod_id() == persisted.pod_id()
        && current.tenant_id() == persisted.tenant_id()
        && current.space_id() == persisted.space_id()
        && current.isolation_type() == persisted.isolation_type()
        && current
            .service_type()
            .zip(persisted.service_type())
            .is_some_and(|(left, right)| left.same_container_family(right))
}

fn same_admitted_registration_payload(
    current: &ProjectAndContainerInfo,
    persisted: &ProjectAndContainerInfo,
) -> anyhow::Result<bool> {
    Ok(same_registration_scope(current, persisted)
        && current.request_id() == persisted.request_id()
        && current.latest_session() == persisted.latest_session()
        && current.persistence_identity().sessions == persisted.persistence_identity().sessions
        && current
            .persistence_identity()
            .retired_sessions
            .iter()
            .all(|(sid, generation)| {
                persisted.persistence_identity().sessions.get(sid) != Some(generation)
            })
        && serde_json::to_value((current.model_provider(), current.status()))?
            == serde_json::to_value((persisted.model_provider(), persisted.status()))?)
}

impl crate::pg::PgStore {
    /// A prior ordinary same-workload update may leave a shared project with
    /// refreshed physical hints and an older persistence identity. Read its real
    /// SQL registration, including a detached binding, instead of inventing an
    /// old Pod snapshot or waiting for the background mirror sync.
    pub async fn refresh_project_registration(
        &self,
        project_id: &str,
    ) -> anyhow::Result<Option<Arc<ProjectAndContainerInfo>>> {
        let deadline = tokio::time::Instant::now() + self.durable_commit_budget();
        tokio::time::timeout_at(
            deadline,
            self.refresh_project_registration_before(project_id, deadline),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("Project registration refresh exceeded the durable deadline")
        })?
    }

    async fn refresh_project_registration_before(
        &self,
        project_id: &str,
        deadline: tokio::time::Instant,
    ) -> anyhow::Result<Option<Arc<ProjectAndContainerInfo>>> {
        if let Some(current) = self.inner.get(project_id) {
            self.reconcile_rejected_container_intent(
                &crate::adapter::container_entry_key(&current),
                deadline,
            )
            .await?;
        }
        anyhow::ensure!(
            self.resolve_project_stop(project_id).await?,
            "Project Stop is awaiting durable confirmation"
        );
        let (mut baseline, mut baseline_containers) = {
            let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            (
                self.inner
                    .get(project_id)
                    .map(|info| HashMap::from([(project_id.to_owned(), info)]))
                    .unwrap_or_default(),
                self.container_registration
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone(),
            )
        };
        let id = project_id.to_owned();
        let previous_registration = baseline.get(project_id).cloned();
        let (fetched, retired_generation, missing_retired) =
            super::database::read(&self.database, move |tx| {
                Box::pin(async move {
                    let Some(fetched) = repo::fetch_project_registration(tx, &id).await? else {
                        // 缺行本身不授权清缓存。旧 project 与精确物理 container
                        // 都已被本系统退休，才证明这是 peer 删除后的旧投影。
                        let retired = if let Some(previous) = previous_registration {
                            let project_retired = repo::project_generation_is_retired(
                                tx,
                                &id,
                                &previous.persistence_identity().generation,
                            )
                            .await?;
                            let container_retired =
                                match previous.persistence_identity().container.as_ref() {
                                    Some(identity) => match identity.physical_uid.as_deref() {
                                        Some(uid) => {
                                            repo::container_generation_is_retired(
                                                tx,
                                                &crate::adapter::container_entry_key(&previous),
                                                &identity.generation,
                                                uid,
                                            )
                                            .await?
                                        }
                                        None => false,
                                    },
                                    None => false,
                                };
                            project_retired && container_retired
                        } else {
                            false
                        };
                        return Ok((None, None, retired));
                    };
                    let retired_generation = if let Some(previous) = previous_registration
                        && previous.persistence_identity().generation != fetched.0.generation
                    {
                        let generation = &previous.persistence_identity().generation;
                        let retired_project =
                            repo::project_generation_is_retired(tx, &id, generation).await?;
                        let retired_container = match (
                            previous.persistence_identity().container.as_ref(),
                            previous.container_info(),
                            fetched.1.as_ref(),
                        ) {
                            (Some(identity), Some(old), Some(target)) => {
                                if let Some(physical_uid) = identity.physical_uid.as_deref() {
                                    old.container_name == target.container_name
                                        && target.logical_id == previous.container_key()
                                        && repo::container_generation_is_retired(
                                            tx,
                                            &old.container_name,
                                            &identity.generation,
                                            physical_uid,
                                        )
                                        .await?
                                } else {
                                    false
                                }
                            }
                            (None, None, _) => true,
                            _ => false,
                        };
                        let containers = fetched
                            .1
                            .clone()
                            .map(|row| HashMap::from([(row.container_name.clone(), row)]))
                            .unwrap_or_default();
                        let persisted = hydrate_project(&fetched.0, &containers)?;
                        let same_instance_stop = super::project_stop::may_rebase_stop_successor(
                            tx, &previous, &persisted,
                        )
                        .await?;
                        (same_instance_stop || (retired_project && retired_container))
                            .then(|| generation.clone())
                    } else {
                        None
                    };
                    Ok((Some(fetched), retired_generation, false))
                })
            })
            .await
            .context("Refresh persistent project registration")?;
        let Some(fetched) = fetched else {
            if missing_retired && let Some(previous) = baseline.get(project_id) {
                let mut registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
                if self
                    .inner
                    .get(project_id)
                    .is_some_and(|current| Arc::ptr_eq(previous, &current))
                {
                    let name = crate::adapter::container_entry_key(previous);
                    let mut containers = self
                        .container_registration
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    if containers.get(&name) == baseline_containers.get(&name) {
                        self.inner.remove_if_generation(
                            project_id,
                            &previous.persistence_identity().generation,
                        );
                        registration.insert(
                            project_id.to_owned(),
                            previous.persistence_identity().generation.clone(),
                        );
                        let expected_uid = previous
                            .persistence_identity()
                            .container
                            .as_ref()
                            .and_then(|identity| identity.physical_uid.as_deref());
                        if self.inner.prune_container_mirror(&name, expected_uid)
                            || self.inner.registered_container_by_key(&name).is_none()
                        {
                            containers.remove(&name);
                        }
                    }
                }
            }
            return Ok(None);
        };
        if let Some(row) = fetched.1.as_ref()
            && baseline_containers
                .get(&row.container_name)
                .is_some_and(|old| {
                    old.generation != row.container_generation
                        || old.physical_uid != row.container_id
                })
        {
            // The deleting replica intentionally retains its old cursor until
            // SQL settles. It must still join a proven peer-built successor
            // immediately, without waiting for periodic mirror sync.
            let read = self
                .read_authoritative_container_binding(&row.container_name, project_id, deadline)
                .await?;
            if read
                .canonical_container
                .as_ref()
                .map(|row| row.persistence_identity())
                != Some(row.persistence_identity())
            {
                return Ok(self.inner.get(project_id));
            }
            let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            if let Err(error) = self.reconcile_authoritative_binding(&read) {
                if error
                    .downcast_ref::<super::authoritative::AuthoritativeVerificationRequired>()
                    .is_some()
                {
                    return Ok(self.inner.get(project_id));
                }
                return Err(error);
            }
            baseline = self
                .inner
                .get(project_id)
                .map(|info| HashMap::from([(project_id.to_owned(), info)]))
                .unwrap_or_default();
            baseline_containers = self
                .container_registration
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
        }
        self.hydrate_project_read(
            ProjectRead::Registration {
                retired_generation: retired_generation.as_deref(),
            },
            fetched,
            &baseline,
            &baseline_containers,
        )
    }

    pub async fn is_session_registration_committed(
        &self,
        expected: &ProjectAndContainerInfo,
        session_id: &str,
    ) -> anyhow::Result<bool> {
        let captured_generation = expected
            .persistence_identity()
            .sessions
            .get(session_id)
            .cloned();
        let local_generation = {
            let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            self.inner.get(expected.project_id()).and_then(|current| {
                current
                    .persistence_identity()
                    .sessions
                    .get(session_id)
                    .cloned()
            })
        };
        let id = session_id.to_owned();
        let checked_captured = captured_generation.clone();
        let Some((persisted, retired)) = super::database::read(&self.database, move |tx| {
            Box::pin(async move {
                let Some((project, container)) = repo::fetch_project_by_session(tx, &id).await?
                else {
                    return Ok(None);
                };
                let containers = container
                    .map(|row| HashMap::from([(row.container_name.clone(), row)]))
                    .unwrap_or_default();
                let persisted = hydrate_project(&project, &containers)?;
                let canonical = super::persist_ops::ContainerSessionAppend::from_registration(
                    &persisted, &persisted, &id,
                )?;
                let mut retired = !repo::session_execution_is_current(tx, &canonical).await?;
                let generations = std::iter::once(canonical.session_generation.as_str())
                    .chain(checked_captured.as_deref())
                    .chain(local_generation.as_deref())
                    .collect::<std::collections::BTreeSet<_>>();
                for generation in generations {
                    retired |= repo::session_generation_is_retired(tx, &id, generation).await?;
                }
                Ok(Some((persisted, retired)))
            })
        })
        .await
        .context("Confirm durable session registration")?
        else {
            return Ok(false);
        };
        if retired || !crate::adapter::session_container_matches(expected, &persisted) {
            return Ok(false);
        }
        let session_generation = persisted.persistence_identity().sessions.get(session_id);
        if expected
            .persistence_identity()
            .retired_sessions
            .get(session_id)
            == session_generation
        {
            return Ok(false);
        }
        let registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
        if registration.get(expected.project_id())
            == Some(&expected.persistence_identity().generation)
        {
            return Ok(false);
        }
        let current = self.inner.get(expected.project_id());
        if let Some(current) = current.as_ref()
            && (!crate::adapter::session_container_matches(expected, current)
                || current
                    .persistence_identity()
                    .retired_sessions
                    .get(session_id)
                    != expected
                        .persistence_identity()
                        .retired_sessions
                        .get(session_id)
                || current
                    .persistence_identity()
                    .retired_sessions
                    .get(session_id)
                    == session_generation
                || current
                    .persistence_identity()
                    .sessions
                    .get(session_id)
                    .is_some_and(|generation| Some(generation) != session_generation))
        {
            return Ok(false);
        }
        if captured_generation
            .as_ref()
            .is_some_and(|generation| Some(generation) != session_generation)
        {
            // A cancelled lookup may have left an uncommitted local UUID in
            // the captured view. Only an already-corrected current membership,
            // with all captured/local/canonical retirements checked above, may
            // confirm the SQL incarnation. This does not perform a boolean-only
            // repair or publish old metadata.
            return Ok(current.as_ref().is_some_and(|current| {
                current.persistence_identity().sessions.get(session_id) == session_generation
                    && current
                        .persistence_identity()
                        .retired_sessions
                        .get(session_id)
                        == expected
                            .persistence_identity()
                            .retired_sessions
                            .get(session_id)
            }));
        }
        Ok(true)
    }

    /// 按 session_id 读：内存镜像 hit 直接返回；miss 回源直查主库一次
    /// （所有副本连 `-rw` 主库，无复制延迟——正常路径 durable 提交后必中），
    /// 命中则 hydrate 进本地镜像（旁路持久化，此后走内存）。
    pub async fn get_by_session_id_with_fetch(
        &self,
        session_id: &str,
    ) -> Option<Arc<ProjectAndContainerInfo>> {
        match self.get_by_session_id_with_fetch_result(session_id).await {
            Ok(info) => info,
            Err(error) => {
                tracing::warn!(%session_id, %error, "Persistent session lookup failed");
                None
            }
        }
    }

    pub async fn get_by_session_id_with_fetch_result(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Option<Arc<ProjectAndContainerInfo>>> {
        if SessionHint::classify(Some(session_id))
            .as_session_id()
            .is_none()
        {
            return Ok(None);
        }
        if let Some(hit) = self.inner.get_by_session_id(session_id) {
            return Ok(Some(hit));
        }
        let (baseline, baseline_containers) = {
            let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            (
                self.inner.iter().into_iter().collect::<HashMap<_, _>>(),
                self.container_registration
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone(),
            )
        };
        let id = session_id.to_owned();
        let Some(fetched) = super::database::read(&self.database, move |tx| {
            Box::pin(async move { repo::fetch_project_by_session(tx, &id).await })
        })
        .await
        .context("Read persistent session routing")?
        else {
            return Ok(None);
        };
        self.hydrate_fetched(session_id, fetched, &baseline, &baseline_containers)
    }

    /// 回源行组装为 info 并旁路写入内存镜像（数据本就来自 PG，回写不走持久化，
    /// 与 sync.rs 的 hydrate 同模式——复用 load 的组装逻辑）。
    /// `fetched_by` 是触发回源的 session_id（可能与 latest_session 不同）——
    /// 必须一并补进镜像，否则该 session 键下次查询仍 miss（回源缓存失效）
    pub(super) fn hydrate_fetched(
        &self,
        fetched_by: &str,
        (project_row, container_row): (ProjectRow, Option<ContainerRow>),
        baseline: &HashMap<String, Arc<ProjectAndContainerInfo>>,
        baseline_containers: &HashMap<
            String,
            shared_types::persistence::ContainerPersistenceIdentity,
        >,
    ) -> anyhow::Result<Option<Arc<ProjectAndContainerInfo>>> {
        self.hydrate_project_read(
            ProjectRead::Session(fetched_by),
            (project_row, container_row),
            baseline,
            baseline_containers,
        )
    }

    #[cfg(test)]
    pub(super) fn hydrate_registration_fetched(
        &self,
        fetched: (ProjectRow, Option<ContainerRow>),
        baseline: &HashMap<String, Arc<ProjectAndContainerInfo>>,
        baseline_containers: &HashMap<
            String,
            shared_types::persistence::ContainerPersistenceIdentity,
        >,
    ) -> anyhow::Result<Option<Arc<ProjectAndContainerInfo>>> {
        self.hydrate_project_read(
            ProjectRead::Registration {
                retired_generation: None,
            },
            fetched,
            baseline,
            baseline_containers,
        )
    }

    fn hydrate_project_read(
        &self,
        read: ProjectRead<'_>,
        (project_row, container_row): (ProjectRow, Option<ContainerRow>),
        baseline: &HashMap<String, Arc<ProjectAndContainerInfo>>,
        baseline_containers: &HashMap<
            String,
            shared_types::persistence::ContainerPersistenceIdentity,
        >,
    ) -> anyhow::Result<Option<Arc<ProjectAndContainerInfo>>> {
        let registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
        let current = self.inner.get(&project_row.project_id);
        let unchanged = match (baseline.get(&project_row.project_id), current.as_ref()) {
            (None, None) => true,
            (Some(previous), Some(current)) => Arc::ptr_eq(previous, current),
            _ => false,
        };
        let mut container_by_name = HashMap::new();
        if let Some(row) = container_row {
            let name = row.container_name.clone();
            container_by_name.insert(name, row);
        }
        // Corrupt or mismatched persistence is an error, never a routable placeholder.
        let mut info = hydrate_project(&project_row, &container_by_name)?;
        if let ProjectRead::Session(session_id) = read {
            anyhow::ensure!(
                info.sessions().contains(session_id),
                "Fetched session identity missing from snapshot"
            );
        }
        // A local accepted retirement wins over an older SQL response, even
        // before its immutable delete reaches PostgreSQL.
        if registration.get(&project_row.project_id) == Some(&project_row.generation) {
            return Ok(None);
        }
        if matches!(read, ProjectRead::Registration { .. }) && !unchanged {
            // An admitted local clear, retirement or metadata update wins over
            // a delayed SQL read. The caller can continue from that winning
            // view; a refresh must never undo accepted in-process intent.
            return Ok(current);
        }
        if let Some(existing) = &current {
            let same_generation =
                existing.persistence_identity().generation == project_row.generation;
            if !same_generation && !unchanged {
                return Ok(None);
            }
            if let ProjectRead::Session(session_id) = read
                && same_generation
                && (existing
                    .persistence_identity()
                    .retired_sessions
                    .get(session_id)
                    == info.persistence_identity().sessions.get(session_id)
                    || existing
                        .persistence_identity()
                        .sessions
                        .get(session_id)
                        .is_some_and(|generation| {
                            Some(generation) != info.persistence_identity().sessions.get(session_id)
                        }))
            {
                return Ok(None);
            }
            // Metadata changes are harmless; a physical-instance change during
            // the database read must never route an old session to its successor.
            if !unchanged && !crate::adapter::session_container_matches(&info, existing) {
                return Ok(None);
            }
        } else if baseline.contains_key(&project_row.project_id) {
            return Ok(None);
        }
        let mut containers = self
            .container_registration
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for (name, row) in &container_by_name {
            if let Some(current) = containers.get(name) {
                let different_instance = current.generation != row.container_generation
                    || current
                        .physical_uid
                        .as_ref()
                        .is_some_and(|uid| row.container_id.as_ref() != Some(uid));
                // A locally admitted successor may predate the SQL fetch but
                // commit while its old response is in flight. Comparing only
                // cursor changes since admission misses that legitimate race.
                if (current.predecessor.as_ref() == Some(&row.container_generation)
                    && different_instance)
                    || (baseline_containers.get(name) != Some(current) && different_instance)
                {
                    return Ok(None);
                }
            }
        }
        let pending = self.pending_ops.load(std::sync::atomic::Ordering::Acquire) != 0;
        let registration_pending_safe = if let ProjectRead::Registration { retired_generation } =
            read
        {
            match current.as_ref() {
                None => true,
                Some(current)
                    if current.persistence_identity().generation == project_row.generation =>
                {
                    current.persistence_identity().revision <= project_row.row_revision
                        && same_admitted_registration_payload(current, &info)?
                }
                Some(current) => {
                    // A retired source (or an uncommitted Stop successor whose
                    // source is retired) may join the exact same physical
                    // project epoch, or a previously proven container successor.
                    retired_generation == Some(current.persistence_identity().generation.as_str())
                        && same_registration_scope(current, &info)
                        && (super::project_stop::same_stop_scope(current, &info)
                            || current
                                .persistence_identity()
                                .container
                                .as_ref()
                                .is_some_and(|old| {
                                    container_by_name.iter().any(|(name, row)| {
                                        containers.get(name).is_some_and(|target| {
                                            target.generation == row.container_generation
                                                && target.physical_uid == row.container_id
                                                && row.container_generation != old.generation
                                        })
                                    })
                                }))
                }
            }
        } else {
            false
        };
        if pending && matches!(read, ProjectRead::Registration { .. }) && !registration_pending_safe
        {
            return Ok(current);
        }
        let revision_allows_publish = current.as_ref().is_none_or(|current| match read {
            ProjectRead::Registration { .. }
                if current.persistence_identity().generation != project_row.generation =>
            {
                registration_pending_safe
            }
            _ => current.persistence_identity().revision <= project_row.row_revision,
        });
        let can_publish = unchanged
            && (!pending || registration_pending_safe)
            && revision_allows_publish
            && container_by_name.iter().all(|(name, row)| {
                containers.get(name).is_none_or(|current| {
                    current.generation == row.container_generation
                        && current.revision <= row.row_revision
                        && current
                            .physical_uid
                            .as_ref()
                            .is_none_or(|uid| row.container_id.as_ref() == Some(uid))
                })
            });
        if matches!(read, ProjectRead::Registration { .. }) && !can_publish {
            // A registration caller subsequently performs a guarded write. Do
            // not return a projection that was unsafe to admit over its local
            // source; continue from the winning local view instead.
            return Ok(current);
        }
        if can_publish
            && matches!(read, ProjectRead::Registration { .. })
            && let Some(current) = current.as_ref()
            && current.persistence_identity().generation == project_row.generation
        {
            let mut identity = info.persistence_identity().clone();
            identity
                .retired_sessions
                .extend(current.persistence_identity().retired_sessions.clone());
            info.set_persistence_identity(identity);
            info.set_timestamps(
                info.created_at(),
                current.last_activity().max(info.last_activity()),
            );
        }
        let info = Arc::new(info);
        if can_publish {
            self.inner.insert_with_session(
                info.project_id().to_owned(),
                Arc::clone(&info),
                None,
            )?;
            for (name, row) in &container_by_name {
                containers.insert(name.clone(), row.persistence_identity());
            }
        }
        // Cache admission is optional. An unrelated pending write or a harmless
        // project metadata update cannot turn a validated SQL result into 404.
        Ok(Some(info))
    }
}
