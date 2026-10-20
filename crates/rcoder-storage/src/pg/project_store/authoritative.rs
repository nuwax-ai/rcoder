//! 注册门禁的权威快照：DB（containers 行 + tombstone）是容器绑定的唯一
//! 裁决依据，内存镜像只是缓存、不参与正确性判断。
//!
//! 背景（多副本事故）：peer 副本 cleaner/stop 删除容器（PG 行 + tombstone）
//! 只清自己的内存镜像；本副本镜像残留旧 workload UID，曾把"本系统已退役的
//! 自我换代"误判为外部替换，导致 `/computer/chat` 永久 500。durable 注册
//! 入口采用惰性验证（[`GateAuthority`]）：常规路径零额外 DB 读，仅当镜像
//! 判定 workload 替换时读权威快照裁决一次。ProjectStore 同步 trait 入口
//! （write-behind）无法 await，沿用镜像比对并由 [`super::sync`] 的容器
//! 删除对账在同步周期内收敛。
//!
//! 快照为 [`AuthoritativeContainerBinding::RetiredByUs`] 时，durable 入口
//! 先调 [`PgStore::retire_peer_deleted_binding`] 把本地残留（容器注册表、
//! 适配器镜像、该容器的 project 镜像）一并失效——否则旧代次身份会让 PG 侧
//! 换代 CAS（行已删 + expected_revision≠0）整单 Superseded，首次注册仍
//! 无法落库。

use crate::pg::PgStore;
use shared_types::persistence::ContainerPersistenceIdentity;

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
#[derive(Clone, Debug)]
pub(in crate::pg) struct AuthoritativeRead {
    pub(in crate::pg) name: String,
    pub(in crate::pg) binding: AuthoritativeContainerBinding,
    based_on: Option<ContainerPersistenceIdentity>,
}

/// 门禁裁决依据。常规注册路径零额外 DB 读；只有镜像判定 workload 替换时，
/// durable 入口才升级到权威快照裁决一次。
#[derive(Debug)]
pub(in crate::pg) enum GateAuthority {
    /// durable 入口首轮：镜像 workload mismatch 时返回
    /// [`AuthoritativeVerificationRequired`]，由调用方读权威快照后重试。
    Deferred,
    /// 权威快照在场：门禁按 DB 值四象限裁决（本系统已退役放行 / 真外部
    /// 替换拒绝 / 异常缺失拒绝 / 绑定一致放行）。
    Snapshot(AuthoritativeRead),
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
    /// await）。不做排空屏障：同副本在途写不会被误判——本副本自己的删除会
    /// 先同步清理本地注册表（门禁分支不再触达），自己的注册以本地镜像为准、
    /// PG 侧 CAS 与 rejected-intent 对账兜底；tombstone 只裁决 peer 副本的
    /// durable 删除这一外部事实，它不可能被本副本在途写伪造或掩盖。
    pub(in crate::pg) async fn read_authoritative_container_binding(
        &self,
        container_name: &str,
    ) -> anyhow::Result<AuthoritativeRead> {
        let based_on = self
            .container_registration
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(container_name)
            .cloned();
        let name = container_name.to_string();
        let name_for_query = name.clone();
        let based_on_for_query = based_on.clone();
        let binding = super::database::read(&self.database, move |tx| {
            Box::pin(async move {
                if let Some(row) =
                    super::repo::fetch_container_registration(tx, &name_for_query).await?
                {
                    return Ok(AuthoritativeContainerBinding::Bound {
                        workload_uid: row.workload_uid,
                    });
                }
                // 行不存在：用本地旧代次查 tombstone 区分本系统退役与异常缺失。
                // mismatch 门分支必然有本地旧条目（`current = Some(previous)`），
                // 全新注册走 fresh 路径不读本快照，故 `based_on` 缺失按异常处理。
                let retired = match based_on_for_query.as_ref() {
                    Some(identity) => {
                        let (generation, physical_uid) = match (
                            identity.generation.as_str(),
                            identity.physical_uid.as_deref(),
                        ) {
                            (generation, Some(physical_uid))
                                if !generation.is_empty() && !physical_uid.is_empty() =>
                            {
                                (generation, physical_uid)
                            }
                            _ => {
                                return Ok(AuthoritativeContainerBinding::AnomalousAbsent);
                            }
                        };
                        super::repo::container_generation_is_retired(
                            tx,
                            &name_for_query,
                            generation,
                            physical_uid,
                        )
                        .await?
                    }
                    None => false,
                };
                if retired {
                    Ok(AuthoritativeContainerBinding::RetiredByUs)
                } else {
                    Ok(AuthoritativeContainerBinding::AnomalousAbsent)
                }
            })
        })
        .await?;
        Ok(AuthoritativeRead {
            name,
            binding,
            based_on,
        })
    }

    /// 权威快照证明旧绑定已由 peer 副本退役时，失效本地残留的注册表条目、
    /// 适配器镜像与该容器的全部 project 镜像（均为 peer 已删除的旧代次）。
    /// 一致性护栏：本地注册表条目仍是快照依据的旧身份才执行；快照之后本地
    /// 已发生换代/新写入则不动，交由 PG 侧 CAS 与下轮 sync 收敛。
    pub(in crate::pg) fn retire_peer_deleted_binding(&self, read: &AuthoritativeRead) {
        let container_id = read.based_on.as_ref().and_then(|i| i.physical_uid.clone());
        {
            let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            let mut registrations = self
                .container_registration
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            match (&read.based_on, registrations.get(&read.name)) {
                (Some(expected), Some(current)) if current == expected => {}
                _ => return,
            }
            registrations.remove(&read.name);
            // project 镜像移除必须与注册表失效同锁：否则与并发注册存在
            // 竞态（收集后、移除前的新写入会被误删）。
            let stale_projects: Vec<String> = match &container_id {
                Some(id) => self
                    .inner
                    .get_projects_by_container_id(id)
                    .iter()
                    .map(|info| info.project_id().to_string())
                    .collect(),
                None => Vec::new(),
            };
            for project_id in stale_projects {
                self.inner.remove(&project_id);
            }
        }
        self.inner
            .prune_container_mirror(&read.name, container_id.as_deref());
    }
}
