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

impl crate::pg::PgStore {
    pub async fn is_session_registration_committed(
        &self,
        expected: &ProjectAndContainerInfo,
        session_id: &str,
    ) -> anyhow::Result<bool> {
        let id = session_id.to_owned();
        let Some((project, container)) = super::database::read(&self.database, move |tx| {
            Box::pin(async move { repo::fetch_project_by_session(tx, &id).await })
        })
        .await
        .context("Confirm durable session registration")?
        else {
            return Ok(false);
        };
        let containers = container
            .map(|row| HashMap::from([(row.container_name.clone(), row)]))
            .unwrap_or_default();
        let persisted = hydrate_project(&project, &containers)?;
        if !crate::adapter::session_container_matches(expected, &persisted) {
            return Ok(false);
        }
        let session_generation = persisted.persistence_identity().sessions.get(session_id);
        if expected
            .persistence_identity()
            .sessions
            .get(session_id)
            .is_some_and(|expected| Some(expected) != session_generation)
        {
            return Ok(false);
        }
        let registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
        if registration.get(expected.project_id())
            == Some(&expected.persistence_identity().generation)
        {
            return Ok(false);
        }
        if let Some(current) = self.inner.get(expected.project_id())
            && (!crate::adapter::session_container_matches(expected, &current)
                || current
                    .persistence_identity()
                    .retired_sessions
                    .get(session_id)
                    == session_generation
                || current
                    .persistence_identity()
                    .sessions
                    .get(session_id)
                    .is_some_and(|current| Some(current) != session_generation))
        {
            return Ok(false);
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
        let info = hydrate_project(&project_row, &container_by_name)?;
        anyhow::ensure!(
            info.sessions().contains(fetched_by),
            "Fetched session identity missing from snapshot"
        );
        let fetched_generation = info.persistence_identity().sessions.get(fetched_by);
        // A local accepted retirement wins over an older SQL response, even
        // before its immutable delete reaches PostgreSQL.
        if registration.get(&project_row.project_id) == Some(&project_row.generation) {
            return Ok(None);
        }
        if let Some(existing) = &current {
            let same_generation =
                existing.persistence_identity().generation == project_row.generation;
            if !same_generation && !unchanged {
                return Ok(None);
            }
            if same_generation
                && (existing
                    .persistence_identity()
                    .retired_sessions
                    .get(fetched_by)
                    == fetched_generation
                    || existing
                        .persistence_identity()
                        .sessions
                        .get(fetched_by)
                        .is_some_and(|generation| Some(generation) != fetched_generation))
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
        let can_publish = unchanged
            && self.pending_ops.load(std::sync::atomic::Ordering::Acquire) == 0
            && current.as_ref().is_none_or(|current| {
                current.persistence_identity().revision <= project_row.row_revision
            })
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
