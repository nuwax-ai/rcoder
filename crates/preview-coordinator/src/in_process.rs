//! 进程内预览权威存储（Compose 单节点 / 单元测试）。
//!
//! 单进程内全量互斥（`tokio::sync::Mutex`）——没有跨进程读者，受理/端口分配/
//! CAS 的串行化天然成立，语义与 PG 事务实现一一对应（由 `contract_suite` 双跑
//! 保证不漂移）。不持久化：进程重启后注册表清空，实例真相由启动对账与心跳
//! 语义在协调器层收敛（Compose 单节点下本机重启即全部实例死亡，与容器行为一致）。
use std::collections::BTreeMap;

use shared_types::{
    AcceptStartInput, AcceptStartOutcome, PREVIEW_PORT_MAX, PREVIEW_PORT_MIN,
    PREVIEW_PORT_RESERVED_MAX, PREVIEW_PORT_RESERVED_MIN, PreviewInstanceRecord,
    PreviewInstanceState, PreviewLifecycleStore, PreviewOperationKind, PreviewOperationRecord,
    PreviewOperationState, PreviewStoreError as Error, is_preview_port,
};

#[derive(Default)]
struct State {
    instances: BTreeMap<String, PreviewInstanceRecord>,
    operations: BTreeMap<String, PreviewOperationRecord>,
}

/// 进程内权威存储。
pub struct InProcessPreviewStore {
    state: tokio::sync::Mutex<State>,
}

impl Default for InProcessPreviewStore {
    fn default() -> Self {
        Self::new()
    }
}

impl InProcessPreviewStore {
    pub fn new() -> Self {
        Self {
            state: tokio::sync::Mutex::new(State::default()),
        }
    }

    fn occupied_ports(state: &State) -> Vec<u16> {
        state
            .instances
            .values()
            .filter(|r| r.state.is_active())
            .filter_map(|r| r.port)
            .collect()
    }

    /// 单锁内完成条件变更并返回变更后快照（避免二次加锁的观测窗口）。
    async fn mutate(
        &self,
        preview_key: &str,
        actor: impl FnOnce(&mut PreviewInstanceRecord) -> Result<(), Error>,
    ) -> Result<PreviewInstanceRecord, Error> {
        let mut state = self.state.lock().await;
        let Some(record) = state.instances.get_mut(preview_key) else {
            return Err(Error::Invalid(format!("preview {preview_key} not found")));
        };
        actor(record)?;
        Ok(record.clone())
    }
}

fn conflict(context: &str, record: &PreviewInstanceRecord) -> Error {
    Error::Conflict(format!(
        "{context}: state={:?}, revision={}",
        record.state, record.revision
    ))
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

#[async_trait::async_trait]
impl PreviewLifecycleStore for InProcessPreviewStore {
    async fn accept_start(&self, input: AcceptStartInput) -> Result<AcceptStartOutcome, Error> {
        let mut state = self.state.lock().await;
        let mut next_revision = 1_i64;
        if let Some(record) = state.instances.get(&input.preview_key) {
            match record.state {
                PreviewInstanceState::Ready => {
                    return Ok(AcceptStartOutcome::ExistingReady(record.clone()));
                }
                PreviewInstanceState::Starting | PreviewInstanceState::Stopping => {
                    return Ok(AcceptStartOutcome::Blocked(record.clone()));
                }
                PreviewInstanceState::Unknown => {
                    let Some(evidence) = input.recover_unknown_evidence.as_deref() else {
                        return Ok(AcceptStartOutcome::Blocked(record.clone()));
                    };
                    let target = record.clone();
                    let record = state
                        .instances
                        .get_mut(&input.preview_key)
                        .expect("row observed under lock");
                    record.state = PreviewInstanceState::Stopped;
                    record.pid = None;
                    record.detail = Some(evidence.to_string());
                    record.updated_at = now();
                    next_revision = target.revision + 1;
                }
                PreviewInstanceState::Stopped | PreviewInstanceState::Failed => {
                    next_revision = record.revision + 1;
                }
            }
        }

        let occupied = Self::occupied_ports(&state);
        let port = match input.requested_port {
            Some(requested) => {
                if !is_preview_port(requested) {
                    return Err(Error::Invalid(format!(
                        "requested preview port {requested} outside pool or reserved"
                    )));
                }
                if occupied.contains(&requested) {
                    return Err(Error::Invalid(format!(
                        "requested preview port {requested} occupied by an active instance"
                    )));
                }
                requested
            }
            None => (PREVIEW_PORT_MIN..=PREVIEW_PORT_MAX)
                .filter(|p| {
                    !(PREVIEW_PORT_RESERVED_MIN..=PREVIEW_PORT_RESERVED_MAX).contains(p)
                        && !occupied.contains(p)
                })
                .min()
                .ok_or_else(|| Error::Invalid("preview port pool exhausted".into()))?,
        };

        let timestamp = now();
        let record = PreviewInstanceRecord {
            preview_key: input.preview_key.clone(),
            project_id: input.project_id.clone(),
            project_path: input.project_path.clone(),
            instance_id: input.instance_id.clone(),
            revision: next_revision,
            operation_id: input.operation_id.clone(),
            host_id: input.host.host_id.clone(),
            pod_name: input.host.pod_name.clone(),
            pod_ip: input.host.pod_ip.clone(),
            pid: None,
            port: Some(port),
            base_path: None,
            state: PreviewInstanceState::Starting,
            last_heartbeat_at: None,
            last_activity_at: timestamp,
            detail: input.recover_unknown_evidence.clone(),
            updated_at: timestamp,
        };
        state.operations.insert(
            input.operation_id.clone(),
            PreviewOperationRecord {
                operation_id: input.operation_id.clone(),
                preview_key: input.preview_key.clone(),
                kind: PreviewOperationKind::Start,
                state: PreviewOperationState::Accepted,
                host_id: input.host.host_id.clone(),
                requested_port: input.requested_port,
                allocated_port: Some(port),
                result: None,
                created_at: timestamp,
                updated_at: timestamp,
            },
        );
        state.instances.insert(input.preview_key, record.clone());
        Ok(AcceptStartOutcome::Admitted(record))
    }

    async fn publish_running(
        &self,
        preview_key: &str,
        operation_id: &str,
        revision: i64,
        pid: i64,
        port: u16,
        base_path: Option<&str>,
    ) -> Result<PreviewInstanceRecord, Error> {
        let mut state = self.state.lock().await;
        {
            let Some(record) = state.instances.get_mut(preview_key) else {
                return Err(Error::Invalid(format!("preview {preview_key} not found")));
            };
            if record.operation_id != operation_id
                || record.revision != revision
                || record.state != PreviewInstanceState::Starting
            {
                return Err(conflict("publish_running lost the race", record));
            }
            record.state = PreviewInstanceState::Ready;
            record.pid = Some(pid);
            record.port = Some(port);
            record.base_path = base_path.map(str::to_string);
            record.last_heartbeat_at = Some(now());
            record.last_activity_at = now();
            record.updated_at = now();
        }
        if let Some(op) = state.operations.get_mut(operation_id) {
            op.state = PreviewOperationState::Succeeded;
            op.result = Some(format!("pid={pid} port={port}"));
            op.updated_at = now();
        }
        Ok(state
            .instances
            .get(preview_key)
            .cloned()
            .expect("record was just updated"))
    }

    async fn accept_stop(
        &self,
        preview_key: &str,
        operation_id: &str,
    ) -> Result<PreviewInstanceRecord, Error> {
        let mut state = self.state.lock().await;
        let Some(record) = state.instances.get(preview_key) else {
            return Err(Error::Invalid(format!(
                "preview {preview_key} has no instance to stop"
            )));
        };
        if !record.state.is_active() {
            return Ok(record.clone());
        }
        let (host_id, port) = (record.host_id.clone(), record.port);
        let record = state
            .instances
            .get_mut(preview_key)
            .expect("row observed under lock");
        record.state = PreviewInstanceState::Stopping;
        record.operation_id = operation_id.to_string();
        record.revision += 1;
        record.updated_at = now();
        let timestamp = now();
        state.operations.insert(
            operation_id.to_string(),
            PreviewOperationRecord {
                operation_id: operation_id.to_string(),
                preview_key: preview_key.to_string(),
                kind: PreviewOperationKind::Stop,
                state: PreviewOperationState::Accepted,
                host_id,
                requested_port: None,
                allocated_port: port,
                result: None,
                created_at: timestamp,
                updated_at: timestamp,
            },
        );
        Ok(state
            .instances
            .get(preview_key)
            .cloned()
            .expect("record was just updated"))
    }

    async fn mark_stopped(
        &self,
        preview_key: &str,
        operation_id: &str,
        revision: i64,
    ) -> Result<PreviewInstanceRecord, Error> {
        let mut state = self.state.lock().await;
        {
            let Some(record) = state.instances.get_mut(preview_key) else {
                return Err(Error::Invalid(format!("preview {preview_key} not found")));
            };
            if record.operation_id == operation_id
                && record.revision == revision
                && record.state == PreviewInstanceState::Stopping
            {
                record.state = PreviewInstanceState::Stopped;
                record.pid = None;
                record.updated_at = now();
            } else if record.state == PreviewInstanceState::Stopped {
                // 并发 stop 已完成 → 幂等成功。
            } else {
                return Err(conflict("mark_stopped lost the race", record));
            }
        }
        if let Some(op) = state.operations.get_mut(operation_id) {
            op.state = PreviewOperationState::Succeeded;
            op.result = Some("stopped".into());
            op.updated_at = now();
        }
        Ok(state
            .instances
            .get(preview_key)
            .cloned()
            .expect("record was just updated"))
    }

    async fn mark_failed(
        &self,
        preview_key: &str,
        instance_id: &str,
        revision: i64,
        detail: &str,
    ) -> Result<PreviewInstanceRecord, Error> {
        let mut state = self.state.lock().await;
        {
            let Some(record) = state.instances.get_mut(preview_key) else {
                return Err(Error::Invalid(format!("preview {preview_key} not found")));
            };
            if record.instance_id != instance_id
                || record.revision != revision
                || !matches!(
                    record.state,
                    PreviewInstanceState::Starting | PreviewInstanceState::Ready
                )
            {
                return Err(conflict("mark_failed lost the race", record));
            }
            record.state = PreviewInstanceState::Failed;
            record.pid = None;
            record.detail = Some(detail.to_string());
            record.updated_at = now();
        }
        for op in state.operations.values_mut() {
            if op.preview_key == preview_key
                && matches!(
                    op.state,
                    PreviewOperationState::Accepted | PreviewOperationState::Running
                )
            {
                op.state = PreviewOperationState::Failed;
                op.result = Some(detail.to_string());
                op.updated_at = now();
            }
        }
        Ok(state
            .instances
            .get(preview_key)
            .cloned()
            .expect("record was just updated"))
    }

    async fn mark_unknown(
        &self,
        preview_key: &str,
        instance_id: &str,
        detail: &str,
    ) -> Result<PreviewInstanceRecord, Error> {
        self.mutate(preview_key, |record| {
            if record.instance_id != instance_id || !record.state.is_active() {
                return Err(conflict("mark_unknown lost the race", record));
            }
            record.state = PreviewInstanceState::Unknown;
            record.detail = Some(detail.to_string());
            record.updated_at = now();
            Ok(())
        })
        .await
    }

    async fn resolve_unknown_stopped(
        &self,
        preview_key: &str,
        instance_id: &str,
        evidence: &str,
    ) -> Result<PreviewInstanceRecord, Error> {
        self.mutate(preview_key, |record| {
            if record.instance_id != instance_id || record.state != PreviewInstanceState::Unknown {
                return Err(conflict("resolve_unknown_stopped lost the race", record));
            }
            record.state = PreviewInstanceState::Stopped;
            record.pid = None;
            record.detail = Some(evidence.to_string());
            record.updated_at = now();
            Ok(())
        })
        .await
    }

    async fn refresh_heartbeat(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> Result<Option<PreviewInstanceRecord>, Error> {
        let mut state = self.state.lock().await;
        let Some(record) = state.instances.get_mut(preview_key) else {
            return Ok(None);
        };
        if record.instance_id != instance_id
            || !matches!(
                record.state,
                PreviewInstanceState::Ready | PreviewInstanceState::Unknown
            )
        {
            return Ok(None);
        }
        record.last_heartbeat_at = Some(now());
        record.updated_at = now();
        Ok(Some(record.clone()))
    }

    async fn flush_activity(
        &self,
        entries: &[shared_types::ActivityFlushEntry],
    ) -> Result<usize, Error> {
        let mut state = self.state.lock().await;
        let mut updated = 0;
        for entry in entries {
            let Some(record) = state.instances.get_mut(&entry.preview_key) else {
                continue;
            };
            if !record.state.is_active() || record.instance_id != entry.instance_id {
                continue;
            }
            if entry.at > record.last_activity_at {
                record.last_activity_at = entry.at;
                updated += 1;
            }
        }
        Ok(updated)
    }

    async fn get(&self, preview_key: &str) -> Result<Option<PreviewInstanceRecord>, Error> {
        Ok(self.state.lock().await.instances.get(preview_key).cloned())
    }

    async fn find_active_by_port(&self, port: u16) -> Result<Option<PreviewInstanceRecord>, Error> {
        Ok(self
            .state
            .lock()
            .await
            .instances
            .values()
            .filter(|r| r.state.is_active() && r.port == Some(port))
            .max_by_key(|r| r.revision)
            .cloned())
    }

    async fn active_ports(&self) -> Result<Vec<u16>, Error> {
        let state = self.state.lock().await;
        Ok(Self::occupied_ports(&state))
    }

    async fn list_by_host(
        &self,
        host_id: &str,
        active_only: bool,
    ) -> Result<Vec<PreviewInstanceRecord>, Error> {
        Ok(self
            .state
            .lock()
            .await
            .instances
            .values()
            .filter(|r| r.host_id == host_id && (!active_only || r.state.is_active()))
            .cloned()
            .collect())
    }

    async fn list_active(&self) -> Result<Vec<PreviewInstanceRecord>, Error> {
        Ok(self
            .state
            .lock()
            .await
            .instances
            .values()
            .filter(|r| r.state.is_active())
            .cloned()
            .collect())
    }

    async fn reconcile_host_reboot(
        &self,
        pod_uid: &str,
        boot_id: &str,
    ) -> Result<Vec<PreviewInstanceRecord>, Error> {
        let prefix = format!("{pod_uid}:");
        let current_host = format!("{pod_uid}:{boot_id}");
        let mut state = self.state.lock().await;
        let mut reconciled = Vec::new();
        for record in state.instances.values_mut() {
            if record.host_id.starts_with(&prefix)
                && record.host_id != current_host
                && record.state.is_active()
            {
                record.state = PreviewInstanceState::Stopped;
                record.pid = None;
                record.detail = Some(format!("host reboot reconciled by {current_host}"));
                record.updated_at = now();
                reconciled.push(record.clone());
            }
        }
        Ok(reconciled)
    }
}
