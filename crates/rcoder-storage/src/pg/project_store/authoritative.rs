//! 注册门禁的权威快照：DB（containers 行 + tombstone）是容器绑定的唯一
//! 裁决依据，内存镜像只是缓存、不参与正确性判断。
//!
//! 背景（多副本事故）：peer 副本 cleaner/stop 删除容器（PG 行 + tombstone）
//! 只清自己的内存镜像；本副本镜像残留旧 workload UID，曾把"本系统已退役的
//! 自我换代"误判为外部替换，导致 `/computer/chat` 永久 500。durable 注册
//! 入口采用惰性验证（[`GateAuthority`]）：常规路径零额外 DB 读，仅当镜像
//! 判定 workload 替换或绑定游标缺少镜像时读取权威快照；本地游标在读取期间
//! 改变则在原 deadline 内重读。ProjectStore 同步 trait 入口
//! （write-behind）无法 await，沿用镜像比对并由 [`super::sync`] 的容器
//! 删除对账在同步周期内收敛。
//!
//! 旧精确代次已退休时，durable 入口通过
//! [`PgStore::reconcile_authoritative_binding`] 同锁失效旧投影并恢复 DB 中
//! 仍存活的项目、会话和已由 peer 重建的容器，防止旧持久身份或残留游标让
//! 新源码请求永久 Superseded。退休项目的会话不复制到新项目代次。

use crate::pg::PgStore;
use shared_types::{ProjectAndContainerInfo, persistence::ContainerPersistenceIdentity};
use std::{collections::HashMap, sync::Arc};
use tokio::time::Instant;

/// 一次注册时刻的权威容器绑定状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::pg) enum AuthoritativeContainerBinding {
    /// containers 行存在；`workload_uid` 为行内登记值（Docker 族行可为 `None`）。
    Bound { workload_uid: Option<String> },
    /// 行不存在，且旧绑定（name, generation, physical_uid）已由本系统退役
    /// （container_tombstones 在案）——同名重建属自我换代，放行。
    RetiredByUs,
    /// 行不存在且无 tombstone：rcoder 的删除/换代路径均会留 tombstone，
    /// 该组合意味着手工动库等异常状态，不得静默放行。
    AnomalousAbsent,
}

/// 权威读取结果：绑定状态 + 读取所依据的本地旧身份（供失效时一致性校验）。
#[derive(Clone)]
pub(in crate::pg) struct AuthoritativeRead {
    pub(in crate::pg) name: String,
    pub(in crate::pg) binding: AuthoritativeContainerBinding,
    based_on: Option<ContainerPersistenceIdentity>,
    retired_projects: HashMap<String, String>,
    refreshed_projects: HashMap<String, Arc<ProjectAndContainerInfo>>,
    project_baselines: HashMap<String, Arc<ProjectAndContainerInfo>>,
    pub(in crate::pg) canonical_container: Option<super::repo::ContainerRow>,
    previous_retired: bool,
}

/// 门禁裁决依据。常规注册路径零额外 DB 读；只有镜像判定 workload 替换时，
/// durable 入口才升级到权威快照裁决。
pub(in crate::pg) enum GateAuthority {
    /// durable 入口首轮：镜像 workload mismatch 时返回
    /// [`AuthoritativeVerificationRequired`]，由调用方读权威快照后重试。
    Deferred,
    /// 权威快照在场：门禁按 DB 值四象限裁决（本系统已退役放行 / 真外部
    /// 替换拒绝 / 异常缺失拒绝 / 绑定一致放行）。
    Snapshot(Box<AuthoritativeRead>),
    /// 同步 trait 入口（write-behind，无法 await）与上游已验证的换代路径：
    /// 镜像 mismatch 直接拒绝（历史行为），镜像由 sync 对账收敛。
    MirrorOnly,
}

/// 内部标记：镜像判定同名 workload 被替换，需要权威快照裁决。
#[derive(Debug, thiserror::Error)]
#[error("container workload binding requires authoritative verification")]
pub(in crate::pg) struct AuthoritativeVerificationRequired {
    pub(in crate::pg) name: String,
}

impl PgStore {
    /// 单只读事务取权威绑定。必须在持有 registration 锁之前调用（内部含
    /// await）。不做排空屏障：只有 DB 中精确旧代次的 tombstone 才证明退休；
    /// 读取期间本地游标或项目 Arc 变化时不应用旧快照。自己的删除保留的游标
    /// 与 peer 删除留下的投影遵守同一验证规则，PG CAS 继续保护读后竞争。
    pub(in crate::pg) async fn read_authoritative_container_binding(
        &self,
        container_name: &str,
        project_id: &str,
        deadline: Instant,
    ) -> anyhow::Result<AuthoritativeRead> {
        let (based_on, project_baselines, retired_generation) = {
            let registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            let based_on = self
                .container_registration
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(container_name)
                .cloned();
            let projects = self
                .inner
                .iter()
                .into_iter()
                .filter(|(_, info)| crate::adapter::container_entry_key(info) == container_name)
                .collect::<HashMap<_, _>>();
            (based_on, projects, registration.get(project_id).cloned())
        };
        let name = container_name.to_owned();
        let query_name = name.clone();
        let query_based_on = based_on.clone();
        let mut projects: Vec<_> = project_baselines
            .iter()
            .map(|(id, info)| {
                (
                    id.clone(),
                    Some(info.persistence_identity().generation.clone()),
                )
            })
            .collect();
        if !project_baselines.contains_key(project_id) {
            projects.push((project_id.to_owned(), retired_generation));
        }
        let (binding, retired_projects, refreshed_projects, canonical_container, previous_retired) =
            tokio::time::timeout_at(
                deadline,
                super::database::read(&self.database, move |tx| {
                    Box::pin(async move {
                        let row =
                            super::repo::fetch_container_registration(tx, &query_name).await?;
                        let previous_retired = match query_based_on.as_ref() {
                            Some(identity)
                                if row.as_ref().is_none_or(|row| {
                                    row.container_generation != identity.generation
                                        || row.container_id != identity.physical_uid
                                }) =>
                            {
                                match identity.physical_uid.as_deref() {
                                    Some(uid)
                                        if !identity.generation.is_empty() && !uid.is_empty() =>
                                    {
                                        super::repo::container_generation_is_retired(
                                            tx,
                                            &query_name,
                                            &identity.generation,
                                            uid,
                                        )
                                        .await?
                                    }
                                    Some(_) | None => false,
                                }
                            }
                            Some(_) | None => false,
                        };
                        let binding = match row.as_ref() {
                            Some(row) => AuthoritativeContainerBinding::Bound {
                                workload_uid: row.workload_uid.clone(),
                            },
                            None if previous_retired => AuthoritativeContainerBinding::RetiredByUs,
                            None => AuthoritativeContainerBinding::AnomalousAbsent,
                        };
                        let mut retired_projects = HashMap::new();
                        let mut refreshed_projects = HashMap::new();
                        if previous_retired {
                            for (id, generation) in projects {
                                if let Some(generation) = generation
                                    && super::repo::project_generation_is_retired(
                                        tx,
                                        &id,
                                        &generation,
                                    )
                                    .await?
                                {
                                    retired_projects.insert(id.clone(), generation);
                                }
                                if let Some((project, container)) =
                                    super::repo::fetch_project_registration(tx, &id).await?
                                {
                                    let containers = container
                                        .map(|row| {
                                            HashMap::from([(row.container_name.clone(), row)])
                                        })
                                        .unwrap_or_default();
                                    refreshed_projects.insert(
                                        id,
                                        Arc::new(super::load::hydrate_project(
                                            &project,
                                            &containers,
                                        )?),
                                    );
                                }
                            }
                        }
                        Ok((
                            binding,
                            retired_projects,
                            refreshed_projects,
                            row,
                            previous_retired,
                        ))
                    })
                }),
            )
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Authoritative container binding read exceeded the durable deadline"
                )
            })??;
        Ok(AuthoritativeRead {
            name,
            binding,
            based_on,
            retired_projects,
            refreshed_projects,
            project_baselines,
            canonical_container,
            previous_retired,
        })
    }

    /// 调用方持有 registration 锁：快照验证、旧镜像失效与新请求准入必须是
    /// 同一临界区。读取期间本地有新写入时重新读权威状态，不能沿用旧裁决。
    pub(in crate::pg) fn prepare_authoritative_info(
        &self,
        mut info: Arc<ProjectAndContainerInfo>,
        registration: &mut HashMap<String, String>,
        authority: &GateAuthority,
    ) -> anyhow::Result<Arc<ProjectAndContainerInfo>> {
        match authority {
            GateAuthority::Deferred | GateAuthority::MirrorOnly => {}
            GateAuthority::Snapshot(read) => {
                let name = crate::adapter::container_entry_key(&info);
                if read.name != name
                    || self
                        .container_registration
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .get(&name)
                        != read.based_on.as_ref()
                {
                    return Err(anyhow::Error::new(AuthoritativeVerificationRequired {
                        name,
                    }));
                }
                self.reconcile_authoritative_binding(read)?;
                if read.previous_retired {
                    if let Some(persisted) = read.refreshed_projects.get(info.project_id()) {
                        let supplied = info.persistence_identity();
                        let proven_source = read.retired_projects.get(info.project_id())
                            == Some(&supplied.generation);
                        anyhow::ensure!(
                            supplied.revision == 0
                                || proven_source
                                || supplied.generation
                                    == persisted.persistence_identity().generation,
                            "Project registration changed; stale snapshot cannot be rebased"
                        );
                        let refreshed = Arc::make_mut(&mut info);
                        refreshed.clear_all_sessions();
                        for (sid, generation) in &persisted.persistence_identity().sessions {
                            refreshed.restore_session_identity(sid, generation.clone());
                        }
                        anyhow::ensure!(
                            refreshed.restore_latest_session(persisted.latest_session()),
                            "Authoritative latest session is not present"
                        );
                        refreshed
                            .set_persistence_identity(persisted.persistence_identity().clone());
                    } else if read.retired_projects.get(info.project_id())
                        == Some(&info.persistence_identity().generation)
                    {
                        // 真实调用方通常从旧镜像 clone 请求，携带已退休的 project/
                        // session 身份。只凭新容器身份不能重用这些已被墓碑封住的值。
                        let previous = info.persistence_identity().generation.clone();
                        let refreshed = Arc::make_mut(&mut info);
                        refreshed.clear_all_sessions();
                        let retired_sessions =
                            refreshed.persistence_identity().retired_sessions.clone();
                        refreshed.set_persistence_identity(
                            shared_types::persistence::ProjectPersistenceIdentity {
                                predecessor: Some(previous),
                                retired_sessions,
                                ..Default::default()
                            },
                        );
                    }
                    for (id, generation) in &read.retired_projects {
                        registration.insert(id.clone(), generation.clone());
                    }
                }
            }
        }
        self.prepare_info(info, registration, authority)
    }

    /// Caller holds registration. A proven retired predecessor may join the
    /// exact DB snapshot, including a successor already rebuilt on a peer.
    pub(in crate::pg) fn reconcile_authoritative_binding(
        &self,
        read: &AuthoritativeRead,
    ) -> anyhow::Result<()> {
        let container_id = read.based_on.as_ref().and_then(|i| i.physical_uid.clone());
        {
            let mut registrations = self
                .container_registration
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if registrations.get(&read.name) != read.based_on.as_ref() {
                return Err(anyhow::Error::new(AuthoritativeVerificationRequired {
                    name: read.name.clone(),
                }));
            }
            if !read.previous_retired {
                return Ok(());
            }
            // project 镜像移除必须与注册表失效同锁：否则与并发注册存在
            // 竞态（收集后、移除前的新写入会被误删）。
            let stale_projects: HashMap<_, _> = self
                .inner
                .iter()
                .into_iter()
                .filter(|(_, info)| crate::adapter::container_entry_key(info) == read.name)
                .collect();
            if stale_projects.len() != read.project_baselines.len()
                || stale_projects.iter().any(|(id, info)| {
                    read.project_baselines
                        .get(id)
                        .is_none_or(|baseline| !Arc::ptr_eq(baseline, info))
                })
            {
                return Err(anyhow::Error::new(AuthoritativeVerificationRequired {
                    name: read.name.clone(),
                }));
            }
            if self.pending_ops.load(std::sync::atomic::Ordering::Acquire) != 0 {
                for (id, persisted) in &read.refreshed_projects {
                    let unsettled_local_change = match read.project_baselines.get(id) {
                        Some(local) => {
                            let local_sessions = local.sessions();
                            let persisted_sessions = persisted.sessions();
                            let retired_live_session = local
                                .persistence_identity()
                                .retired_sessions
                                .keys()
                                .any(|sid| persisted_sessions.contains(sid));
                            local_sessions != persisted_sessions
                                || local.latest_session() != persisted.latest_session()
                                || local.request_id() != persisted.request_id()
                                || serde_json::to_value((local.model_provider(), local.status()))?
                                    != serde_json::to_value((
                                        persisted.model_provider(),
                                        persisted.status(),
                                    ))?
                                || retired_live_session
                        }
                        None => read.retired_projects.contains_key(id),
                    };
                    if unsettled_local_change {
                        // A known retired predecessor permits cache recovery,
                        // but cannot discard an accepted clear/removal or new
                        // request while its immutable write is still unsettled.
                        // Unrelated pending work with the same payload is safe;
                        // migrated session UUIDs alone are not a local mutation.
                        return Err(anyhow::Error::new(AuthoritativeVerificationRequired {
                            name: read.name.clone(),
                        }));
                    }
                }
            }
            for (project_id, info) in stale_projects {
                self.inner
                    .remove_if_generation(&project_id, &info.persistence_identity().generation);
            }
            anyhow::ensure!(
                self.inner
                    .prune_container_mirror(&read.name, container_id.as_deref()),
                "Retired container still has a live local project reference; no replacement was admitted"
            );
            registrations.remove(&read.name);
            if let Some(row) = read.canonical_container.as_ref() {
                let service_type = row
                    .service_type
                    .parse::<shared_types::ServiceType>()
                    .map_err(anyhow::Error::msg)?;
                self.inner.restore_unreferenced_container(
                    super::load::container_row_to_basic(row),
                    service_type,
                )?;
                registrations.insert(read.name.clone(), row.persistence_identity());
            }
            for (id, info) in &read.refreshed_projects {
                self.inner.insert(id.clone(), Arc::clone(info))?;
            }
        }
        Ok(())
    }
}
