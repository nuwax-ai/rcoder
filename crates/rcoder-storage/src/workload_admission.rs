//! Workload admission is independent of SQL registration locks and physical deletion.
use anyhow::{Result, ensure};
use shared_types::{ServiceType, persistence::PersistenceWriteOutcome};
use std::sync::Arc;
use tokio::time::Instant;

pub(crate) mod memory;
#[cfg(feature = "pg")]
pub(crate) mod pg;
#[cfg(test)]
mod tests;

/// Callers use the actual canonical workload name, not a project sharing it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkloadKey {
    namespace: String,
    family: ServiceType,
    logical_name: String,
}
impl WorkloadKey {
    pub fn new(namespace: &str, family: ServiceType, logical_name: &str) -> Result<Self> {
        ensure!(!namespace.trim().is_empty(), "Workload namespace is empty");
        ensure!(
            !logical_name.trim().is_empty(),
            "Canonical workload name is empty"
        );
        Ok(Self {
            namespace: namespace.into(),
            family: family.family_representative(),
            logical_name: logical_name.into(),
        })
    }
    pub fn namespace(&self) -> &str {
        &self.namespace
    }
    pub fn family(&self) -> ServiceType {
        self.family
    }
    pub fn logical_name(&self) -> &str {
        &self.logical_name
    }
    #[cfg(feature = "pg")]
    fn lock_name(&self) -> String {
        // Length-delimited fields avoid delimiter ambiguity. The hash seed is
        // deliberately distinct from writer::lock_ops and leader election.
        format!(
            "agent-admission:{}:{}:{}:{}:{}",
            self.namespace.len(),
            self.namespace,
            self.family.container_family_key(),
            self.logical_name.len(),
            self.logical_name
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkloadAdmissionMode {
    Shared,
    Exclusive,
}

/// 准入会话核验失败的具体来源。
///
/// `OutcomeUnknown` 是只读 SELECT 探针的回复未知（oneshot 通道被丢弃），
/// 不是 SQL 写入结果未知；类型明确不等于可重试，也不据此放行未知执行。
#[derive(Debug, thiserror::Error)]
pub enum AdmissionVerifyError {
    #[error("Workload admission session lost")]
    SessionLost,
    #[error("Workload admission session closed")]
    SessionClosed,
    #[error("Workload admission verify outcome is unknown")]
    OutcomeUnknown,
    #[error("Workload admission verify probe failed")]
    ProbeFailed(#[source] anyhow::Error),
}

pub struct WorkloadAdmissionGuard {
    key: WorkloadKey,
    mode: WorkloadAdmissionMode,
    owner: Arc<()>,
    inner: Guard,
}
enum Guard {
    Memory(memory::MemoryGuard),
    #[cfg(feature = "pg")]
    Postgres(pg::PgGuard),
}
impl WorkloadAdmissionGuard {
    pub fn key(&self) -> &WorkloadKey {
        &self.key
    }
    pub fn mode(&self) -> WorkloadAdmissionMode {
        self.mode
    }
    /// 扁平 anyhow 视图（既有调用方）；需要区分来源的路径用 [`Self::verify_current_typed`]。
    pub async fn verify_current(&self) -> Result<()> {
        self.verify_current_typed()
            .await
            .map_err(anyhow::Error::new)
    }
    /// 类型化核验：区分会话丢失/关闭、回复未知与探针本身失败。
    pub async fn verify_current_typed(&self) -> Result<(), AdmissionVerifyError> {
        match &self.inner {
            Guard::Memory(_) => Ok(()),
            #[cfg(feature = "pg")]
            Guard::Postgres(guard) => guard.verify().await,
        }
    }
    /// Uses the very same pinned session which owns the admission lock.
    /// Builder lifecycle/operation/binding checks remain the caller's duty.
    pub async fn has_canonical_references(&self) -> Result<bool> {
        match &self.inner {
            Guard::Memory(guard) => Ok(guard.has_references(&self.key)),
            #[cfg(feature = "pg")]
            Guard::Postgres(guard) => guard.has_references().await,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContainerRegistration {
    pub(crate) family: String,
    pub(crate) generation: String,
    pub(crate) revision: i64,
    pub(crate) physical_uid: Option<String>,
    pub(crate) workload_uid: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectRegistration {
    pub(crate) id: String,
    pub(crate) user_id: Option<String>,
    pub(crate) generation: String,
    pub(crate) revision: i64,
    pub(crate) container_generation: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionRegistration {
    pub(crate) id: String,
    pub(crate) generation: String,
    pub(crate) project_id: String,
    pub(crate) project_generation: String,
}

/// A pre-physical-action registration snapshot. Absence is preserved as absence;
/// it can never authorize retiring a subsequently registered physical object.
#[derive(Clone)]
pub struct WorkloadRegistrationFrame {
    pub(crate) key: WorkloadKey,
    pub(crate) owner: Arc<()>,
    pub(crate) container_name: String,
    pub(crate) container: Option<ContainerRegistration>,
    pub(crate) projects: Vec<ProjectRegistration>,
    pub(crate) sessions: Vec<SessionRegistration>,
    pub(crate) memory: Vec<Arc<shared_types::ProjectAndContainerInfo>>,
}
impl WorkloadRegistrationFrame {
    pub fn container_name(&self) -> &str {
        &self.container_name
    }
    pub fn physical_uid(&self) -> Option<&str> {
        self.container
            .as_ref()
            .and_then(|row| row.physical_uid.as_deref())
    }
    pub fn workload_uid(&self) -> Option<&str> {
        self.container
            .as_ref()
            .and_then(|row| row.workload_uid.as_deref())
    }
    pub fn project_ids(&self) -> Vec<&str> {
        self.projects.iter().map(|row| row.id.as_str()).collect()
    }
    pub fn session_ids(&self) -> Vec<&str> {
        self.sessions.iter().map(|row| row.id.as_str()).collect()
    }
    pub fn user_ids(&self) -> Vec<&str> {
        self.projects
            .iter()
            .filter_map(|row| row.user_id.as_deref())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

impl crate::ProjectStoreBackend {
    pub async fn acquire_workload_admission(
        &self,
        key: WorkloadKey,
        mode: WorkloadAdmissionMode,
        deadline: Instant,
    ) -> Result<WorkloadAdmissionGuard> {
        match self {
            Self::Memory(store) => memory::acquire(store.clone(), key, mode, deadline).await,
            #[cfg(feature = "pg")]
            Self::Postgres(store) => store.workload_admission.acquire(key, mode, deadline).await,
        }
    }
    fn validate_guard(&self, guard: &WorkloadAdmissionGuard) -> Result<()> {
        let owner = match self {
            Self::Memory(store) => &store.workload_admission.owner,
            #[cfg(feature = "pg")]
            Self::Postgres(store) => &store.workload_admission.owner,
        };
        ensure!(
            Arc::ptr_eq(owner, &guard.owner),
            "Workload guard belongs to another storage instance"
        );
        Ok(())
    }
    pub async fn capture_workload_registration(
        &self,
        guard: &WorkloadAdmissionGuard,
        container_name: &str,
    ) -> Result<WorkloadRegistrationFrame> {
        self.validate_guard(guard)?;
        ensure!(
            container_name == guard.key.logical_name,
            "Registration name differs from guarded workload"
        );
        match &guard.inner {
            Guard::Memory(memory) => {
                memory.capture(&guard.key, guard.owner.clone(), container_name)
            }
            #[cfg(feature = "pg")]
            Guard::Postgres(pg) => pg.capture().await,
        }
    }
    pub async fn retire_workload_registration(
        &self,
        guard: &WorkloadAdmissionGuard,
        frame: &WorkloadRegistrationFrame,
    ) -> Result<PersistenceWriteOutcome> {
        self.validate_guard(guard)?;
        ensure!(
            guard.mode == WorkloadAdmissionMode::Exclusive,
            "Registration retirement requires exclusive workload admission"
        );
        ensure!(
            guard.key == frame.key && Arc::ptr_eq(&guard.owner, &frame.owner),
            "Registration frame differs from guarded workload"
        );
        match &guard.inner {
            Guard::Memory(memory) => memory.retire(frame),
            #[cfg(feature = "pg")]
            Guard::Postgres(pg) => {
                let outcome = pg.retire(frame.clone()).await?;
                if outcome == PersistenceWriteOutcome::Committed {
                    match self {
                        Self::Postgres(store) => store.prune_committed_workload_frame(frame),
                        Self::Memory(_) => {
                            anyhow::bail!("PostgreSQL guard belongs to another backend")
                        }
                    }
                }
                Ok(outcome)
            }
        }
    }
}
