//! Durable writes and queued fallbacks execute identical, identity-fenced operations.
//! SQL transaction order is not business order; retired identities fence delayed retries.
use super::persist_ops::{ContainerSessionAppend, PersistOp, structural_ops_for_insert};
use super::writer::execute_registered;
use crate::adapter::container_entry_key;
use crate::pg::PgStore;
use shared_types::{ProjectAndContainerInfo, SessionHint, persistence::PersistenceWriteOutcome};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// Dropping a cancelled durable request queues its immutable registered operations.
pub(super) struct PendingWrite<'a> {
    store: &'a PgStore,
    ops: Option<Vec<PersistOp>>,
}

enum SessionBinding {
    Missing,
    Bound,
    Rejected,
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
    pub(super) fn register_durable(&self, ops: Vec<PersistOp>) -> PendingWrite<'_> {
        for op in &ops {
            self.register_container_intent(op);
        }
        self.pending_ops
            .fetch_add(ops.len() as i64, std::sync::atomic::Ordering::AcqRel);
        self.active_writes
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        PendingWrite {
            store: self,
            ops: Some(ops),
        }
    }

    pub(super) const DURABLE_COMMIT_TIMEOUT: Duration = Duration::from_millis(600);

    /// Establish the exact accepted project/container registration before a
    /// caller dispatches work. A background writer is not a commit barrier.
    pub async fn insert_project_registration_durable(
        &self,
        project_id: String,
        mut info: Arc<ProjectAndContainerInfo>,
    ) -> anyhow::Result<PersistenceWriteOutcome> {
        let deadline = Instant::now() + Self::DURABLE_COMMIT_TIMEOUT;
        anyhow::ensure!(
            project_id == info.project_id(),
            "Project registration key differs from its payload"
        );
        if let Some(reconciled) = self
            .reconcile_rejected_container_intent(&container_entry_key(&info), deadline)
            .await?
        {
            reconciled.rebase_fresh_info(&mut info);
        }
        match tokio::time::timeout_at(deadline, self.resolve_project_stop(&project_id)).await {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) | Err(_) => {
                return Ok(PersistenceWriteOutcome::Deferred {
                    reason:
                        "Project Stop is awaiting durable confirmation; no new task was dispatched"
                            .into(),
                });
            }
            Ok(Err(error)) => return Err(error),
        }
        // 门禁惰性权威验证：常规路径零额外 DB 读（挂起点与既有语义不变）；
        // 仅当镜像判定同名 workload 被替换时，出锁读权威快照（DB 行 +
        // tombstone）裁决一次——peer 已退役则先失效本地旧代次镜像、以全新
        // 身份重试，保证事故路径（peer 删除 + 本副本镜像残留）首次注册即可
        // 落库。准入（prepare/op/镜像写入/op 登记）与权威读严格分段持锁。
        let mut authority = super::authoritative::GateAuthority::Deferred;
        let (pending, captured) = 'admission: {
            loop {
                // Some = 准入完成（op 已在锁内登记）；None = 需要权威裁决。
                let step = {
                    let mut registration =
                        self.registration.lock().unwrap_or_else(|p| p.into_inner());
                    if self.closing.load(std::sync::atomic::Ordering::Acquire) {
                        return Err(anyhow::anyhow!("Persistence is shutting down"));
                    }
                    match self.prepare_authoritative_info(
                        Arc::clone(&info),
                        &mut registration,
                        &authority,
                    ) {
                        Ok(prepared) => {
                            let op = super::persist_ops::registration_for_metadata(&prepared)?;
                            self.inner
                                .insert(project_id.clone(), Arc::clone(&prepared))?;
                            let captured = self
                                .inner
                                .get(&project_id)
                                .ok_or_else(|| anyhow::anyhow!("Registered project disappeared"))?;
                            Some((self.register_durable(vec![op]), captured))
                        }
                        Err(error) => {
                            let needs_authority = error
                                .downcast_ref::<super::authoritative::AuthoritativeVerificationRequired>()
                                .is_some();
                            if needs_authority {
                                None
                            } else {
                                return Err(error);
                            }
                        }
                    }
                };
                match step {
                    Some(admitted) => break 'admission admitted,
                    None => {
                        let name = container_entry_key(&info);
                        let read = self
                            .read_authoritative_container_binding(&name, &project_id, deadline)
                            .await?;
                        authority = super::authoritative::GateAuthority::Snapshot(read);
                    }
                }
            }
        };
        let outcome = self
            .execute_durable_before(pending, "register_project", deadline)
            .await;
        if outcome == PersistenceWriteOutcome::Superseded {
            // RegisterProject's SQL savepoint rolled back its container write
            // too. Do not let this rejected optimistic cursor permanently block
            // a proven same-physical project-epoch refresh.
            self.record_rejected_container_intent(&captured);
            if let Err(error) = self
                .reconcile_rejected_container_intent(&container_entry_key(&captured), deadline)
                .await
            {
                // The mutation is definitively rejected. Keep that fact and
                // its narrow correction proof for the next fresh entrance;
                // a readback fault never turns Superseded into unknown write.
                tracing::warn!(%error, "Known rejected container cursor readback failed");
            }
        }
        if outcome == PersistenceWriteOutcome::Committed {
            let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            if self.inner.get(captured.project_id()).is_none_or(|current| {
                !crate::adapter::session_container_matches(&captured, &current)
            }) {
                return Ok(PersistenceWriteOutcome::Superseded);
            }
        }
        Ok(outcome)
    }

    pub async fn append_session_for_container_durable(
        &self,
        expected: &ProjectAndContainerInfo,
        session_id: &str,
    ) -> anyhow::Result<PersistenceWriteOutcome> {
        let deadline = Instant::now() + Self::DURABLE_COMMIT_TIMEOUT;
        let (mut pending, mut append, was_present) = {
            let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            anyhow::ensure!(
                !self.closing.load(std::sync::atomic::Ordering::Acquire),
                "Persistence is shutting down"
            );
            let Some(current) = self.inner.get(expected.project_id()) else {
                return Ok(PersistenceWriteOutcome::Superseded);
            };
            let was_present = current.sessions().contains(session_id);
            if !self
                .inner
                .append_session_for_container(expected, session_id)?
            {
                return Ok(PersistenceWriteOutcome::Superseded);
            }
            let current = self.inner.get(expected.project_id()).ok_or_else(|| {
                anyhow::anyhow!("Project disappeared during session registration")
            })?;
            let append = ContainerSessionAppend::from_registration(expected, &current, session_id)?;
            (
                self.register_durable(vec![PersistOp::AppendSessionForContainer(Box::new(
                    append.clone(),
                ))]),
                append,
                was_present,
            )
        };
        // Keep the accepted mirror and immutable fallback through lookup failure
        // or cancellation: Stop must still capture this exact pending generation.
        // Every completion checks SQL, including a provisional SID left by an
        // earlier cancelled lookup. No prompt or uncertain write is replayed.
        let rejected = match tokio::time::timeout_at(
            deadline,
            self.bind_committed_session(expected, &mut append),
        )
        .await
        {
            Ok(Ok(SessionBinding::Bound)) => {
                // The original command has not executed. Bind its routing to
                // the committed incarnation, never to a new guessed SID UUID.
                pending.ops = Some(vec![PersistOp::AppendSessionForContainer(Box::new(
                    append.clone(),
                ))]);
                false
            }
            Ok(Ok(SessionBinding::Missing)) => false,
            Ok(Ok(SessionBinding::Rejected)) => true,
            Ok(Err(error)) => {
                return Ok(PersistenceWriteOutcome::Deferred {
                    reason: format!("Read committed session identity: {error}"),
                });
            }
            Err(_) => {
                return Ok(PersistenceWriteOutcome::Deferred {
                    reason: "Session identity lookup exceeded the durable deadline".into(),
                });
            }
        };
        let outcome = if rejected {
            if let Some(ops) = pending.ops.take() {
                self.pending_ops
                    .fetch_sub(ops.len() as i64, std::sync::atomic::Ordering::AcqRel);
            }
            PersistenceWriteOutcome::Superseded
        } else {
            self.execute_durable_before(pending, "append_session_for_container", deadline)
                .await
        };
        if outcome == PersistenceWriteOutcome::Superseded && !was_present {
            let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(current) = self.inner.get(expected.project_id())
                && crate::adapter::session_container_matches(expected, &current)
                && current.persistence_identity().sessions.get(session_id)
                    == Some(&append.session_generation)
            {
                self.inner
                    .clear_session_one(expected.project_id(), session_id);
            }
        }
        Ok(outcome)
    }

    async fn bind_committed_session(
        &self,
        expected: &ProjectAndContainerInfo,
        append: &mut ContainerSessionAppend,
    ) -> anyhow::Result<SessionBinding> {
        let checked = append.clone();
        let captured_generation = expected
            .persistence_identity()
            .sessions
            .get(&append.session_id)
            .cloned();
        let (fetched, retired) = super::database::read(&self.database, move |tx| {
            Box::pin(async move {
                if !super::repo::session_execution_is_current(tx, &checked).await? {
                    return Ok((None, true));
                }
                let generations = std::iter::once(checked.session_generation.as_str())
                    .chain(captured_generation.as_deref())
                    .collect::<std::collections::BTreeSet<_>>();
                for generation in generations {
                    if super::repo::session_generation_is_retired(
                        tx,
                        &checked.session_id,
                        generation,
                    )
                    .await?
                    {
                        return Ok((None, true));
                    }
                }
                let Some((project, container)) =
                    super::repo::fetch_project_by_session(tx, &checked.session_id).await?
                else {
                    return Ok((None, false));
                };
                let generation = project
                    .session_identities
                    .get(&checked.session_id)
                    .ok_or_else(|| anyhow::anyhow!("Committed SID has no generation"))?;
                let retired =
                    super::repo::session_generation_is_retired(tx, &checked.session_id, generation)
                        .await?;
                Ok((Some((project, container)), retired))
            })
        })
        .await?;
        if retired {
            return Ok(SessionBinding::Rejected);
        }
        let Some((project, container)) = fetched else {
            return Ok(SessionBinding::Missing);
        };
        let containers = container
            .map(|row| std::collections::HashMap::from([(row.container_name.clone(), row)]))
            .unwrap_or_default();
        let persisted = super::load::hydrate_project(&project, &containers)?;
        let generation = persisted
            .persistence_identity()
            .sessions
            .get(&append.session_id)
            .ok_or_else(|| anyhow::anyhow!("Committed SID has no hydrated generation"))?;
        if !crate::adapter::session_container_matches(expected, &persisted) {
            return Ok(SessionBinding::Rejected);
        }
        let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
        let Some(current) = self.inner.get(expected.project_id()) else {
            return Ok(SessionBinding::Rejected);
        };
        let local_generation = current
            .persistence_identity()
            .sessions
            .get(&append.session_id);
        if !crate::adapter::session_container_matches(expected, &current)
            || (local_generation != Some(&append.session_generation)
                && local_generation != Some(generation))
            || current
                .persistence_identity()
                .retired_sessions
                .get(&append.session_id)
                != expected
                    .persistence_identity()
                    .retired_sessions
                    .get(&append.session_id)
            || current
                .persistence_identity()
                .retired_sessions
                .get(&append.session_id)
                == Some(generation)
        {
            return Ok(SessionBinding::Rejected);
        }
        // Only the unretired canonical routing is joined. Keep the winning
        // metadata and any selection made by a newer accepted request.
        let latest = current.latest_session().map(str::to_owned);
        let preserve_latest = current.persistence_identity().revision != append.dispatch_revision
            || current.request_id() != append.request_id.as_deref();
        let mut merged = (*current).clone();
        merged.restore_session_identity(&append.session_id, generation.clone());
        if preserve_latest {
            anyhow::ensure!(
                merged.restore_latest_session(latest.as_deref()),
                "Current session selection disappeared during routing merge"
            );
        }
        self.inner
            .insert(expected.project_id().to_owned(), Arc::new(merged))?;
        append.session_generation = generation.clone();
        Ok(SessionBinding::Bound)
    }

    pub async fn insert_with_session_durable(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
        session_id: &str,
    ) -> anyhow::Result<()> {
        let deadline = Instant::now() + Self::DURABLE_COMMIT_TIMEOUT;
        anyhow::ensure!(
            SessionHint::classify(Some(session_id))
                .as_session_id()
                .is_some(),
            "An established session identity is required"
        );
        // 同 insert_project_registration_durable：镜像 workload 替换时出锁
        // 读权威快照裁决，peer 已退役则失效本地残留后重试一次。
        let mut authority = super::authoritative::GateAuthority::Deferred;
        let ops = 'admission: {
            loop {
                let step = {
                    let mut registration =
                        self.registration.lock().unwrap_or_else(|p| p.into_inner());
                    if self.closing.load(std::sync::atomic::Ordering::Acquire) {
                        return Err(anyhow::anyhow!("Persistence is shutting down"));
                    }
                    match self.prepare_authoritative_info(
                        Arc::clone(&info),
                        &mut registration,
                        &authority,
                    ) {
                        Ok(mut prepared) => {
                            Arc::make_mut(&mut prepared).add_session(session_id);
                            let ops = structural_ops_for_insert(&prepared, session_id)?;
                            self.inner.insert_with_session(
                                project_id.clone(),
                                Arc::clone(&prepared),
                                Some(session_id),
                            )?;
                            Some(self.register_durable(ops))
                        }
                        Err(error) => {
                            let needs_authority = error
                                .downcast_ref::<super::authoritative::AuthoritativeVerificationRequired>()
                                .is_some();
                            if needs_authority {
                                None
                            } else {
                                return Err(error);
                            }
                        }
                    }
                };
                match step {
                    Some(ops) => break 'admission ops,
                    None => {
                        let name = container_entry_key(&info);
                        let read = self
                            .read_authoritative_container_binding(&name, &project_id, deadline)
                            .await?;
                        authority = super::authoritative::GateAuthority::Snapshot(read);
                    }
                }
            }
        };
        self.execute_durable_before(ops, "insert_session", deadline)
            .await;
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
            let Some(current) = self.inner.get(project_id) else {
                return Ok(false);
            };
            let container_generation = current
                .persistence_identity()
                .container
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Container registration identity is missing"))?
                .generation
                .clone();
            let container_name = container_entry_key(&current);
            let Some(_removed) =
                self.inner
                    .remove_if_container_identity(project_id, generation, container_id)
            else {
                return Ok(false);
            };
            registration.insert(project_id.to_string(), generation.to_string());
            self.register_durable(vec![PersistOp::RemoveProjectForContainer {
                project_id: project_id.to_string(),
                generation: generation.to_string(),
                container_id: container_id.to_string(),
                container_name,
                container_generation,
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
        let expected = match self.capture_project_stop_registration(project_id).await {
            Ok(Some(expected)) => expected,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(project_id, %error, "Stop source capture failed");
                return;
            }
        };
        if let Err(error) = self.clear_sessions_if_current_durable(&expected).await {
            tracing::warn!(project_id, %error, "Stop registration failed");
        }
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
            let containers = self.container_deletion_targets(container_id);
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
                    containers,
                    projects,
                }]),
            )
        };
        self.execute_durable(op, "delete_container").await;
        result
    }

    pub(super) async fn execute_durable(
        &self,
        registered: PendingWrite<'_>,
        name: &str,
    ) -> PersistenceWriteOutcome {
        self.execute_durable_before(
            registered,
            name,
            Instant::now() + Self::DURABLE_COMMIT_TIMEOUT,
        )
        .await
    }

    pub(super) async fn execute_durable_before(
        &self,
        mut registered: PendingWrite<'_>,
        name: &str,
        deadline: Instant,
    ) -> PersistenceWriteOutcome {
        let durable =
            execute_registered(&self.database, registered.ops.clone().unwrap_or_default());
        match tokio::time::timeout_at(deadline, durable).await {
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
