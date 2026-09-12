//! Durable writes and queued fallbacks execute identical, identity-fenced operations.
//! SQL transaction order is not business order; retired identities fence delayed retries.
use super::persist_ops::{PersistOp, structural_ops_for_insert};
use super::writer::execute_op;
use crate::adapter::container_entry_key;
use crate::pg::PgStore;
use shared_types::{ProjectAndContainerInfo, persistence::PersistenceWriteOutcome};
use std::sync::Arc;
use std::time::Duration;

/// Dropping a cancelled durable request queues its immutable registered operations.
struct PendingWrite<'a> {
    store: &'a PgStore,
    ops: Option<Vec<PersistOp>>,
}
impl Drop for PendingWrite<'_> {
    fn drop(&mut self) {
        if let Some(ops) = self.ops.take() {
            let count = ops.len() as i64;
            for op in ops {
                self.store.enqueue_structural(op);
            }
            self.store
                .pending_ops
                .fetch_sub(count, std::sync::atomic::Ordering::AcqRel);
        }
        self.store
            .active_writes
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        self.store.write_finished.notify_waiters();
    }
}

impl PgStore {
    fn register_durable(&self, ops: Vec<PersistOp>) -> PendingWrite<'_> {
        self.pending_ops
            .fetch_add(ops.len() as i64, std::sync::atomic::Ordering::AcqRel);
        self.active_writes
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        PendingWrite {
            store: self,
            ops: Some(ops),
        }
    }

    const DURABLE_COMMIT_TIMEOUT: Duration = Duration::from_millis(600);

    pub async fn insert_with_session_durable(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
        session_id: &str,
    ) -> anyhow::Result<()> {
        let ops = {
            let registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            anyhow::ensure!(
                !self.closing.load(std::sync::atomic::Ordering::Acquire),
                "Persistence is shutting down"
            );
            let mut info = self.prepare_info(info, &registration);
            Arc::make_mut(&mut info).add_session(session_id);
            let ops = structural_ops_for_insert(&info, session_id)?;
            self.inner
                .insert_with_session(project_id, info, Some(session_id))?;
            self.register_durable(ops)
        };
        self.execute_durable(ops, "insert_session").await;
        Ok(())
    }

    pub async fn add_session_durable(
        &self,
        project_id: &str,
        session_id: &str,
    ) -> anyhow::Result<bool> {
        let op = {
            let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            anyhow::ensure!(
                !self.closing.load(std::sync::atomic::Ordering::Acquire),
                "Persistence is shutting down"
            );
            if !self.inner.add_session_to_project(project_id, session_id) {
                return Ok(false);
            }
            let info = self.inner.get(project_id).ok_or_else(|| {
                anyhow::anyhow!("Project disappeared during session registration: {project_id}")
            })?;
            let op = PersistOp::AddSession {
                project_id: project_id.to_string(),
                session_id: session_id.to_string(),
                project_generation: info.persistence_identity().generation.clone(),
                predecessor: info
                    .persistence_identity()
                    .retired_sessions
                    .get(session_id)
                    .cloned(),
                generation: info
                    .persistence_identity()
                    .sessions
                    .get(session_id)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Session identity missing: {session_id}"))?,
                container_name: info.container_info().map(|_| container_entry_key(&info)),
            };
            self.register_durable(vec![op])
        };
        self.execute_durable(op, "add_session").await;
        Ok(true)
    }

    pub async fn remove_durable(&self, project_id: &str) -> Option<Arc<ProjectAndContainerInfo>> {
        let (removed, op) = {
            let mut registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            if self.closing.load(std::sync::atomic::Ordering::Acquire) {
                return None;
            }
            let removed = self.inner.remove(project_id)?;
            let generation = removed.persistence_identity().generation.clone();
            registration.insert(project_id.to_string(), generation.clone());
            self.touch_throttled.invalidate(&format!("p:{project_id}"));
            (
                removed,
                self.register_durable(vec![PersistOp::RemoveProject {
                    project_id: project_id.to_string(),
                    generation,
                }]),
            )
        };
        self.execute_durable(op, "remove").await;
        Some(removed)
    }

    pub async fn remove_durable_if_generation(
        &self,
        project_id: &str,
        expected_generation: &str,
    ) -> anyhow::Result<bool> {
        let registered = {
            let mut registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            anyhow::ensure!(
                !self.closing.load(std::sync::atomic::Ordering::Acquire),
                "Persistence is shutting down"
            );
            let Some(removed) = self
                .inner
                .remove_if_generation(project_id, expected_generation)
            else {
                return Ok(false);
            };
            let generation = removed.persistence_identity().generation.clone();
            registration.insert(project_id.to_string(), generation.clone());
            self.register_durable(vec![PersistOp::RemoveProject {
                project_id: project_id.to_string(),
                generation,
            }])
        };
        match self
            .execute_durable(registered, "remove_if_generation")
            .await
        {
            PersistenceWriteOutcome::Committed => Ok(true),
            PersistenceWriteOutcome::Superseded => Ok(false),
            PersistenceWriteOutcome::Deferred { reason } => Err(anyhow::anyhow!(
                "Conditional project removal deferred: {reason}"
            )),
        }
    }

    pub async fn remove_durable_if_container_identity(
        &self,
        project_id: &str,
        generation: &str,
        container_id: &str,
    ) -> anyhow::Result<bool> {
        let registered = {
            let mut registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            anyhow::ensure!(
                !self.closing.load(std::sync::atomic::Ordering::Acquire),
                "Persistence is shutting down"
            );
            let Some(removed) =
                self.inner
                    .remove_if_container_identity(project_id, generation, container_id)
            else {
                return Ok(false);
            };
            let container = removed.container_info().ok_or_else(|| {
                anyhow::anyhow!("Container identity disappeared during conditional removal")
            })?;
            registration.insert(project_id.to_string(), generation.to_string());
            self.register_durable(vec![PersistOp::RemoveProjectForContainer {
                project_id: project_id.to_string(),
                generation: generation.to_string(),
                container_id: container_id.to_string(),
                container_name: container.container_name,
            }])
        };
        match self
            .execute_durable(registered, "remove_if_container_identity")
            .await
        {
            PersistenceWriteOutcome::Committed => Ok(true),
            PersistenceWriteOutcome::Superseded => Ok(false),
            PersistenceWriteOutcome::Deferred { reason } => Err(anyhow::anyhow!(
                "Conditional container project removal deferred: {reason}"
            )),
        }
    }

    pub async fn clear_session_durable(&self, project_id: &str) {
        let op = {
            let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            if self.closing.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            let Some(info) = self.inner.get(project_id) else {
                return;
            };
            let op = PersistOp::ClearSessions {
                project_id: project_id.to_string(),
                generation: info.persistence_identity().generation.clone(),
                sessions: info
                    .persistence_identity()
                    .sessions
                    .iter()
                    .map(|(id, g)| (id.clone(), g.clone()))
                    .collect(),
            };
            self.inner.clear_session(project_id);
            self.register_durable(vec![op])
        };
        self.execute_durable(op, "clear_sessions").await;
    }

    pub async fn clear_session_one_durable(&self, project_id: &str, session_id: &str) -> bool {
        let op = {
            let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            if self.closing.load(std::sync::atomic::Ordering::Acquire) {
                return false;
            }
            let Some(info) = self.inner.get(project_id) else {
                return false;
            };
            let Some(generation) = info
                .persistence_identity()
                .sessions
                .get(session_id)
                .cloned()
            else {
                return false;
            };
            if !self.inner.clear_session_one(project_id, session_id) {
                return false;
            }
            self.touch_throttled.invalidate(&format!("s:{session_id}"));
            self.register_durable(vec![PersistOp::RemoveSession {
                session_id: session_id.to_string(),
                generation,
            }])
        };
        self.execute_durable(op, "remove_session").await;
        true
    }

    pub async fn delete_container_with_projects_durable(
        &self,
        container_id: &str,
    ) -> (bool, usize) {
        let (result, op) = {
            let mut registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            if self.closing.load(std::sync::atomic::Ordering::Acquire) {
                return (false, 0);
            }
            let projects: Vec<_> = self
                .inner
                .get_projects_by_container_id(container_id)
                .iter()
                .map(|p| {
                    (
                        p.project_id().to_string(),
                        p.persistence_identity().generation.clone(),
                    )
                })
                .collect();
            let result = self.inner.delete_container_with_projects(container_id);
            if !result.0 {
                return result;
            }
            for (id, generation) in &projects {
                registration.insert(id.clone(), generation.clone());
            }
            (
                result,
                self.register_durable(vec![PersistOp::DeleteContainerWithProjects {
                    container_id: container_id.to_string(),
                    projects,
                }]),
            )
        };
        self.execute_durable(op, "delete_container").await;
        result
    }

    async fn execute_durable(
        &self,
        mut registered: PendingWrite<'_>,
        name: &str,
    ) -> PersistenceWriteOutcome {
        let durable = async {
            let mut tx = self.pool.begin().await?;
            let mut superseded = 0usize;
            if let Some(ops) = &registered.ops {
                super::writer::lock_ops(&mut tx, ops).await?;
                for op in ops {
                    if execute_op(&mut tx, op).await?
                        == shared_types::persistence::PersistenceOperationOutcome::Superseded
                    {
                        superseded += 1;
                    }
                }
            }
            tx.commit().await?;
            Ok::<_, anyhow::Error>(superseded)
        };
        match tokio::time::timeout(Self::DURABLE_COMMIT_TIMEOUT, durable).await {
            Ok(Ok(superseded)) => {
                if let Some(ops) = registered.ops.take() {
                    tracing::info!(
                        committed = ops.len() - superseded,
                        superseded,
                        "[STORAGE_PG] durable {name} resolved"
                    );
                    self.pending_ops
                        .fetch_sub(ops.len() as i64, std::sync::atomic::Ordering::AcqRel);
                }
                if superseded == 0 {
                    PersistenceWriteOutcome::Committed
                } else {
                    PersistenceWriteOutcome::Superseded
                }
            }
            outcome => {
                let reason = match outcome {
                    Ok(Err(e)) => e.to_string(),
                    Err(_) => "timeout".into(),
                    Ok(Ok(_)) => return PersistenceWriteOutcome::Committed,
                };
                tracing::warn!("[STORAGE_PG] durable {name} deferred: {reason}");
                // PendingWrite queues on drop, also covering request cancellation.
                PersistenceWriteOutcome::Deferred { reason }
            }
        }
    }
}
