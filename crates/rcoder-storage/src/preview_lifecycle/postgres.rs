//! Preview persistence through the official Toasty PostgreSQL driver. The fixed
//! allocation lock covers missing rows as well as different keys sharing a port.
use super::domain::{self, unavailable};
use crate::db::{
    models::{PreviewInstance, PreviewOperation},
    owner::DatabaseOwner,
    schema::Component,
};
use chrono::SubsecRound as _;
use futures::future::BoxFuture;
use sha2::{Digest, Sha256};
use shared_types::*;
use toasty::Executor;
use toasty_core::schema::db::Type;
type Error = PreviewStoreError;

pub struct PgPreviewStore {
    owner: DatabaseOwner,
}
impl PgPreviewStore {
    pub async fn connect(config: &crate::config::PostgresConfig) -> Result<Self, Error> {
        Ok(Self {
            owner: crate::db::postgres::open(config, vec![Component::Preview])
                .await
                .map_err(unavailable)?,
        })
    }
    pub async fn close(&self) -> Result<(), Error> {
        self.owner.shutdown().await.map_err(unavailable)
    }
    async fn run<T, F>(&self, body: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut dyn Executor) -> BoxFuture<'a, Result<T, Error>> + Send + 'static,
    {
        self.owner.execute(move |mut db| async move {
            let mut tx = db.transaction().await?;
            match body(&mut tx).await {
                Ok(value) => { tx.commit().await?; Ok(Ok(value)) }
                Err(error) => {
                    tx.rollback().await.map_err(|rollback| anyhow::anyhow!("Preview transaction failed ({error}); rollback failed ({rollback}); outcome requires verification"))?;
                    Ok(Err(error))
                }
            }
        }).await.map_err(unavailable)?
    }
}
fn conflict(context: &str) -> Error {
    Error::Conflict(context.into())
}
fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now().trunc_subsecs(6)
}
async fn lock_key(tx: &mut dyn Executor, key: &str) -> Result<(), Error> {
    toasty::sql::query("SELECT preview_key FROM preview_instances WHERE preview_key=$1 FOR UPDATE")
        .bind(key)
        .exec(tx)
        .await
        .map_err(unavailable)?;
    Ok(())
}
async fn get(tx: &mut dyn Executor, key: &str) -> Result<Option<PreviewInstanceRecord>, Error> {
    PreviewInstance::filter_by_preview_key(key)
        .first()
        .exec(tx)
        .await
        .map_err(unavailable)?
        .map(domain::record)
        .transpose()
}
async fn current(tx: &mut dyn Executor, key: &str) -> Result<PreviewInstanceRecord, Error> {
    lock_key(tx, key).await?;
    get(tx, key)
        .await?
        .ok_or_else(|| Error::Invalid("Preview has no current instance".into()))
}
async fn save(
    tx: &mut dyn Executor,
    before: &PreviewInstanceRecord,
    row: &PreviewInstanceRecord,
) -> Result<(), Error> {
    let count = toasty::sql::statement("UPDATE preview_instances SET revision=$1,operation_id=$2,pid=$3,port=$4,base_path=$5,state=$6,last_heartbeat_at_us=$7,last_activity_at_us=$8,detail=$9,updated_at_us=$10 WHERE preview_key=$11 AND instance_id=$12 AND revision=$13 AND operation_id=$14 AND state=$15")
        .bind(row.revision).bind(&row.operation_id).bind_typed(row.pid, Type::Integer(8))
        .bind_typed(row.port.map(i64::from), Type::Integer(8)).bind_typed(row.base_path.as_deref(), Type::Text)
        .bind(domain::state_str(row.state)).bind_typed(row.last_heartbeat_at.map(|t| t.timestamp_micros()), Type::Integer(8))
        .bind(row.last_activity_at.timestamp_micros()).bind_typed(row.detail.as_deref(), Type::Text).bind(row.updated_at.timestamp_micros())
        .bind(&before.preview_key).bind(&before.instance_id).bind(before.revision).bind(&before.operation_id).bind(domain::state_str(before.state))
        .exec(tx).await.map_err(unavailable)?;
    if count != 1 {
        return Err(conflict("Preview instance CAS failed"));
    }
    Ok(())
}
async fn operation(tx: &mut dyn Executor, id: &str) -> Result<Option<PreviewOperation>, Error> {
    PreviewOperation::filter_by_operation_id(id)
        .first()
        .exec(tx)
        .await
        .map_err(unavailable)
}
async fn insert_operation(
    tx: &mut dyn Executor,
    row: &PreviewInstanceRecord,
    kind: &str,
    requested: Option<u16>,
    input: serde_json::Value,
) -> Result<(), Error> {
    let request = encode_userapp_intent(&input).map_err(unavailable)?;
    let fingerprint = Sha256::digest(&request)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let request = String::from_utf8(request).map_err(unavailable)?;
    PreviewOperation::create()
        .operation_id(&row.operation_id)
        .preview_key(&row.preview_key)
        .instance_id(&row.instance_id)
        .request_fingerprint(fingerprint)
        .kind(kind)
        .state("accepted")
        .host_id(&row.host_id)
        .requested_port(requested.map(i64::from))
        .allocated_port(row.port.map(i64::from))
        .payload_version(1)
        .request_json(request)
        .result_json(None::<String>)
        .created_at_us(row.updated_at.timestamp_micros())
        .updated_at_us(row.updated_at.timestamp_micros())
        .exec(tx)
        .await
        .map_err(unavailable)?;
    Ok(())
}
async fn finish(
    tx: &mut dyn Executor,
    row: &PreviewInstanceRecord,
    state: &str,
    result: serde_json::Value,
) -> Result<(), Error> {
    let op = operation(tx, &row.operation_id)
        .await?
        .ok_or_else(|| unavailable("Preview operation record missing"))?;
    if op.preview_key != row.preview_key
        || op.instance_id != row.instance_id
        || op.payload_version != 1
    {
        return Err(conflict(
            "Preview operation belongs to a different instance",
        ));
    }
    if matches!(op.state.as_str(), "accepted" | "running" | "uncertain") {
        let count = toasty::sql::statement("UPDATE preview_operations SET state=$1,result_json=$2,updated_at_us=$3 WHERE operation_id=$4 AND instance_id=$5 AND state=$6")
            .bind(state).bind(serde_json::to_string(&result).map_err(unavailable)?).bind(row.updated_at.timestamp_micros())
            .bind(&row.operation_id).bind(&row.instance_id).bind(&op.state).exec(tx).await.map_err(unavailable)?;
        if count != 1 {
            return Err(conflict("Preview operation CAS failed"));
        }
    }
    Ok(())
}
async fn active(tx: &mut dyn Executor) -> Result<Vec<PreviewInstanceRecord>, Error> {
    PreviewInstance::filter(
        PreviewInstance::fields()
            .state()
            .in_list(["starting", "ready", "stopping", "unknown"]),
    )
    .order_by(PreviewInstance::fields().preview_key().asc())
    .exec(tx)
    .await
    .map_err(unavailable)?
    .into_iter()
    .map(domain::record)
    .collect()
}
async fn ports(tx: &mut dyn Executor) -> Result<Vec<u16>, Error> {
    active(tx)
        .await?
        .into_iter()
        .map(|row| {
            row.port
                .ok_or_else(|| unavailable("Active preview port is missing"))
        })
        .collect()
}
fn allocate(occupied: &[u16], requested: Option<u16>) -> Result<u16, Error> {
    if let Some(port) = requested {
        if !is_preview_port(port) || occupied.contains(&port) {
            return Err(Error::Invalid(
                "Requested preview port is outside the pool, reserved, or occupied".into(),
            ));
        }
        return Ok(port);
    }
    (PREVIEW_PORT_MIN..=PREVIEW_PORT_MAX)
        .find(|port| is_preview_port(*port) && !occupied.contains(port))
        .ok_or_else(|| Error::Invalid("Preview port pool exhausted".into()))
}
fn start_input(input: &AcceptStartInput) -> serde_json::Value {
    serde_json::json!({"preview_key":input.preview_key,"project_id":input.project_id,"project_path":input.project_path,
        "instance_id":input.instance_id,"host_id":input.host.host_id,"pod_name":input.host.pod_name,
        "pod_ip":input.host.pod_ip,"requested_port":input.requested_port,"recovery":input.recover_unknown_evidence})
}
async fn accept_start(
    tx: &mut dyn Executor,
    input: AcceptStartInput,
) -> Result<AcceptStartOutcome, Error> {
    if [
        &input.preview_key,
        &input.project_id,
        &input.instance_id,
        &input.operation_id,
        &input.host.host_id,
    ]
    .iter()
    .any(|s| s.is_empty())
    {
        return Err(Error::Invalid("Preview start identity is empty".into()));
    }
    // Independent namespace from schema and leader locks; transaction-scoped,
    // released before returning and never held while spawning or probing a host.
    toasty::sql::query("SELECT 1 FROM pg_advisory_xact_lock(72430591234122)")
        .exec(tx)
        .await
        .map_err(unavailable)?;
    lock_key(tx, &input.preview_key).await?;
    let existing = get(tx, &input.preview_key).await?;
    if let Some(previous) = operation(tx, &input.operation_id).await? {
        let fingerprint =
            Sha256::digest(encode_userapp_intent(&start_input(&input)).map_err(unavailable)?)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
        if previous.kind != "start"
            || previous.preview_key != input.preview_key
            || previous.instance_id != input.instance_id
            || previous.request_fingerprint != fingerprint
            || previous.payload_version != 1
        {
            return Err(conflict(
                "Preview start identity was reused with different input",
            ));
        }
        let row = existing.ok_or_else(|| unavailable("Preview request lost its instance"))?;
        if row.instance_id != input.instance_id || row.operation_id != input.operation_id {
            return Err(conflict("Preview request belongs to an older instance"));
        }
        return Ok(if row.state == PreviewInstanceState::Ready {
            AcceptStartOutcome::ExistingReady(row)
        } else {
            AcceptStartOutcome::Blocked(row)
        });
    }
    let revision = if let Some(row) = &existing {
        match row.state {
            PreviewInstanceState::Ready => {
                return Ok(AcceptStartOutcome::ExistingReady(row.clone()));
            }
            PreviewInstanceState::Starting | PreviewInstanceState::Stopping => {
                return Ok(AcceptStartOutcome::Blocked(row.clone()));
            }
            PreviewInstanceState::Unknown => {
                let Some(evidence) = input.recover_unknown_evidence.as_ref() else {
                    return Ok(AcceptStartOutcome::Blocked(row.clone()));
                };
                if evidence.instance_id != row.instance_id
                    || evidence.revision != row.revision
                    || evidence.detail.is_empty()
                {
                    return Err(conflict(
                        "Unknown recovery evidence belongs to another preview generation",
                    ));
                }
                let mut stopped = row.clone();
                stopped.state = PreviewInstanceState::Stopped;
                stopped.pid = None;
                stopped.detail = Some(evidence.detail.clone());
                stopped.updated_at = now();
                save(tx, row, &stopped).await?;
                finish(
                    tx,
                    &stopped,
                    "failed",
                    serde_json::json!({"recovery":evidence}),
                )
                .await?;
            }
            PreviewInstanceState::Stopped | PreviewInstanceState::Failed => {}
        }
        row.revision
            .checked_add(1)
            .ok_or_else(|| unavailable("Preview revision exhausted"))?
    } else {
        1
    };
    let port = allocate(&ports(tx).await?, input.requested_port)?;
    let at = now();
    if let Some(previous) = &existing {
        // Allocation lock covers creation; row lock/CAS also excludes concurrent
        // host callbacks that do not need the global allocator lock.
        lock_key(tx, &input.preview_key).await?;
        let observed = get(tx, &input.preview_key)
            .await?
            .ok_or_else(|| conflict("Preview disappeared"))?;
        if observed.instance_id != previous.instance_id
            || observed.revision != previous.revision
            || observed.state.is_active()
        {
            return Err(conflict("Preview changed during start admission"));
        }
        let count = toasty::sql::statement("UPDATE preview_instances SET project_id=$1,project_path=$2,instance_id=$3,revision=$4,operation_id=$5,host_id=$6,pod_name=$7,pod_ip=$8,pid=NULL,port=$9,base_path=NULL,state='starting',last_heartbeat_at_us=NULL,last_activity_at_us=$10,detail=$11,updated_at_us=$10 WHERE preview_key=$12 AND instance_id=$13 AND revision=$14")
            .bind(&input.project_id).bind(&input.project_path).bind(&input.instance_id).bind(revision).bind(&input.operation_id).bind(&input.host.host_id)
            .bind_typed(input.host.pod_name.as_deref(), Type::Text).bind_typed(input.host.pod_ip.as_deref(), Type::Text).bind(i64::from(port))
            .bind(at.timestamp_micros()).bind_typed(input.recover_unknown_evidence.as_ref().map(|e| e.detail.clone()), Type::Text).bind(&input.preview_key)
            .bind(&previous.instance_id).bind(previous.revision).exec(tx).await.map_err(unavailable)?;
        if count != 1 {
            return Err(conflict("Preview changed during start admission"));
        }
    } else {
        PreviewInstance::create()
            .preview_key(&input.preview_key)
            .project_id(&input.project_id)
            .project_path(&input.project_path)
            .instance_id(&input.instance_id)
            .revision(revision)
            .operation_id(Some(input.operation_id.clone()))
            .host_id(&input.host.host_id)
            .pod_name(input.host.pod_name.clone())
            .pod_ip(input.host.pod_ip.clone())
            .pid(None::<i64>)
            .port(Some(i64::from(port)))
            .base_path(None::<String>)
            .state("starting")
            .last_heartbeat_at_us(None::<i64>)
            .last_activity_at_us(at.timestamp_micros())
            .detail(
                input
                    .recover_unknown_evidence
                    .as_ref()
                    .map(|e| e.detail.clone()),
            )
            .updated_at_us(at.timestamp_micros())
            .exec(tx)
            .await
            .map_err(unavailable)?;
    }
    let row = get(tx, &input.preview_key)
        .await?
        .ok_or_else(|| unavailable("Admitted preview is missing"))?;
    insert_operation(tx, &row, "start", input.requested_port, start_input(&input)).await?;
    Ok(AcceptStartOutcome::Admitted(row))
}

#[async_trait::async_trait]
impl PreviewLifecycleStore for PgPreviewStore {
    async fn accept_start(&self, input: AcceptStartInput) -> Result<AcceptStartOutcome, Error> {
        self.run(move |tx| Box::pin(accept_start(tx, input))).await
    }
    async fn publish_running(
        &self,
        key: &str,
        operation_id: &str,
        revision: i64,
        pid: i64,
        port: u16,
        base_path: Option<&str>,
    ) -> Result<PreviewInstanceRecord, Error> {
        let key = key.to_owned();
        let operation_id = operation_id.to_owned();
        let base_path = base_path.map(str::to_owned);
        self.run(move |tx| Box::pin(async move {
            let before = current(tx, &key).await?;
            if pid <= 0 || before.operation_id != operation_id || before.revision != revision || before.state != PreviewInstanceState::Starting || before.port != Some(port) {
                return Err(conflict("Publish running requires the original starting operation and allocated port"));
            }
            let mut row = before.clone(); row.state = PreviewInstanceState::Ready; row.pid = Some(pid); row.base_path = base_path;
            row.updated_at = now(); row.last_heartbeat_at = Some(row.updated_at); row.last_activity_at = row.last_activity_at.max(row.updated_at);
            save(tx, &before, &row).await?;
            finish(tx, &row, "succeeded", serde_json::json!({"pid":pid,"port":port})).await?;
            Ok(row)
        })).await
    }
    async fn accept_stop(
        &self,
        key: &str,
        operation_id: &str,
    ) -> Result<PreviewInstanceRecord, Error> {
        let key = key.to_owned();
        let operation_id = operation_id.to_owned();
        self.run(move |tx| {
            Box::pin(async move {
                if operation_id.is_empty() {
                    return Err(Error::Invalid("Stop operation identity is empty".into()));
                }
                let before = current(tx, &key).await?;
                if let Some(op) = operation(tx, &operation_id).await? {
                    if op.kind != "stop"
                        || op.preview_key != key
                        || op.instance_id != before.instance_id
                        || before.operation_id != operation_id
                    {
                        return Err(conflict(
                            "Stop operation belongs to another instance or request",
                        ));
                    }
                    return Ok(before);
                }
                if !before.state.is_active() {
                    return Ok(before);
                }
                if before.state == PreviewInstanceState::Stopping {
                    return Err(conflict("Another stop operation is in progress"));
                }
                let mut row = before.clone();
                row.state = PreviewInstanceState::Stopping;
                row.operation_id = operation_id;
                row.revision = row
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| unavailable("Preview revision exhausted"))?;
                row.updated_at = now();
                // A stop supersedes the startup authority but cannot turn it into a
                // success. The original receipt remains attached to its instance.
                finish(
                    tx,
                    &before,
                    "failed",
                    serde_json::json!({"superseded_by":row.operation_id}),
                )
                .await?;
                save(tx, &before, &row).await?;
                insert_operation(
                    tx,
                    &row,
                    "stop",
                    None,
                    serde_json::json!({"preview_key":key,"instance_id":row.instance_id}),
                )
                .await?;
                Ok(row)
            })
        })
        .await
    }
    async fn mark_stopped(
        &self,
        key: &str,
        operation_id: &str,
        revision: i64,
    ) -> Result<PreviewInstanceRecord, Error> {
        let key = key.to_owned();
        let operation_id = operation_id.to_owned();
        self.run(move |tx| {
            Box::pin(async move {
                let before = current(tx, &key).await?;
                if before.operation_id != operation_id || before.revision != revision {
                    return Err(conflict("Stop completion belongs to another generation"));
                }
                if before.state == PreviewInstanceState::Stopped {
                    return Ok(before);
                }
                if before.state != PreviewInstanceState::Stopping {
                    return Err(conflict("Preview is not stopping"));
                }
                let mut row = before.clone();
                row.state = PreviewInstanceState::Stopped;
                row.pid = None;
                row.updated_at = now();
                save(tx, &before, &row).await?;
                finish(tx, &row, "succeeded", serde_json::json!({"stopped":true})).await?;
                Ok(row)
            })
        })
        .await
    }
    async fn mark_failed(
        &self,
        key: &str,
        instance_id: &str,
        revision: i64,
        detail: &str,
    ) -> Result<PreviewInstanceRecord, Error> {
        let key = key.to_owned();
        let instance_id = instance_id.to_owned();
        let detail = detail.to_owned();
        self.run(move |tx| {
            Box::pin(async move {
                let before = current(tx, &key).await?;
                if before.instance_id != instance_id
                    || before.revision != revision
                    || !matches!(
                        before.state,
                        PreviewInstanceState::Starting | PreviewInstanceState::Ready
                    )
                {
                    return Err(conflict("Failure belongs to another preview generation"));
                }
                let mut row = before.clone();
                row.state = PreviewInstanceState::Failed;
                row.pid = None;
                row.detail = Some(detail.clone());
                row.updated_at = now();
                save(tx, &before, &row).await?;
                finish(tx, &row, "failed", serde_json::json!({"detail":detail})).await?;
                Ok(row)
            })
        })
        .await
    }
    async fn mark_unknown(
        &self,
        key: &str,
        instance_id: &str,
        detail: &str,
    ) -> Result<PreviewInstanceRecord, Error> {
        let key = key.to_owned();
        let instance_id = instance_id.to_owned();
        let detail = detail.to_owned();
        self.run(move |tx| {
            Box::pin(async move {
                let before = current(tx, &key).await?;
                if before.instance_id != instance_id || !before.state.is_active() {
                    return Err(conflict(
                        "Unknown observation belongs to another preview generation",
                    ));
                }
                let mut row = before.clone();
                row.state = PreviewInstanceState::Unknown;
                row.detail = Some(detail.clone());
                row.updated_at = now();
                save(tx, &before, &row).await?;
                finish(tx, &row, "uncertain", serde_json::json!({"detail":detail})).await?;
                Ok(row)
            })
        })
        .await
    }
    async fn resolve_unknown_stopped(
        &self,
        key: &str,
        instance_id: &str,
        evidence: &str,
    ) -> Result<PreviewInstanceRecord, Error> {
        let key = key.to_owned();
        let instance_id = instance_id.to_owned();
        let evidence = evidence.to_owned();
        self.run(move |tx| {
            Box::pin(async move {
                let before = current(tx, &key).await?;
                if evidence.is_empty()
                    || before.instance_id != instance_id
                    || before.state != PreviewInstanceState::Unknown
                {
                    return Err(conflict(
                        "Unknown recovery evidence belongs to another preview generation",
                    ));
                }
                let mut row = before.clone();
                row.state = PreviewInstanceState::Stopped;
                row.pid = None;
                row.detail = Some(evidence.clone());
                row.updated_at = now();
                save(tx, &before, &row).await?;
                let op = operation(tx, &row.operation_id)
                    .await?
                    .ok_or_else(|| unavailable("Missing preview operation"))?;
                finish(
                    tx,
                    &row,
                    if op.kind == "stop" {
                        "succeeded"
                    } else {
                        "failed"
                    },
                    serde_json::json!({"recovery":evidence}),
                )
                .await?;
                Ok(row)
            })
        })
        .await
    }
    async fn refresh_heartbeat(
        &self,
        key: &str,
        instance_id: &str,
    ) -> Result<Option<PreviewInstanceRecord>, Error> {
        let key = key.to_owned();
        let instance_id = instance_id.to_owned();
        self.run(move |tx| {
            Box::pin(async move {
                lock_key(tx, &key).await?;
                let Some(before) = get(tx, &key).await? else {
                    return Ok(None);
                };
                if before.instance_id != instance_id
                    || !matches!(
                        before.state,
                        PreviewInstanceState::Ready | PreviewInstanceState::Unknown
                    )
                {
                    return Ok(None);
                }
                let mut row = before.clone();
                row.last_heartbeat_at = Some(
                    before
                        .last_heartbeat_at
                        .map_or_else(now, |at| at.max(now())),
                );
                save(tx, &before, &row).await?;
                Ok(Some(row))
            })
        })
        .await
    }
    async fn flush_activity(&self, entries: &[ActivityFlushEntry]) -> Result<usize, Error> {
        let mut entries = entries.to_vec();
        entries.sort_by(|a, b| {
            (&a.preview_key, &a.instance_id).cmp(&(&b.preview_key, &b.instance_id))
        });
        self.run(move |tx| Box::pin(async move {
            let mut updated = 0usize;
            for entry in entries {
                let count = toasty::sql::statement("UPDATE preview_instances SET last_activity_at_us=GREATEST(last_activity_at_us,$1) WHERE preview_key=$2 AND instance_id=$3 AND last_activity_at_us<$1 AND state IN ('starting','ready','stopping','unknown')")
                    .bind(entry.at.timestamp_micros()).bind(&entry.preview_key).bind(&entry.instance_id).exec(tx).await.map_err(unavailable)?;
                updated = updated.checked_add(usize::try_from(count).map_err(unavailable)?).ok_or_else(|| unavailable("Preview activity count overflow"))?;
            }
            Ok(updated)
        })).await
    }
    async fn get(&self, key: &str) -> Result<Option<PreviewInstanceRecord>, Error> {
        let key = key.to_owned();
        self.run(move |tx| Box::pin(async move { get(tx, &key).await }))
            .await
    }
    async fn find_active_by_port(&self, port: u16) -> Result<Option<PreviewInstanceRecord>, Error> {
        self.run(move |tx| {
            Box::pin(async move {
                let fields = PreviewInstance::fields();
                PreviewInstance::filter(
                    fields.port().eq(Some(i64::from(port))).and(
                        fields
                            .state()
                            .in_list(["starting", "ready", "stopping", "unknown"]),
                    ),
                )
                .first()
                .exec(tx)
                .await
                .map_err(unavailable)?
                .map(domain::record)
                .transpose()
            })
        })
        .await
    }
    async fn active_ports(&self) -> Result<Vec<u16>, Error> {
        self.run(|tx| Box::pin(ports(tx))).await
    }
    async fn list_by_host(
        &self,
        host_id: &str,
        active_only: bool,
    ) -> Result<Vec<PreviewInstanceRecord>, Error> {
        let host_id = host_id.to_owned();
        self.run(move |tx| {
            Box::pin(async move {
                let rows =
                    PreviewInstance::filter(PreviewInstance::fields().host_id().eq(&host_id))
                        .order_by(PreviewInstance::fields().preview_key().asc())
                        .exec(tx)
                        .await
                        .map_err(unavailable)?
                        .into_iter()
                        .map(domain::record)
                        .collect::<Result<Vec<_>, _>>()?;
                Ok(rows
                    .into_iter()
                    .filter(|r| !active_only || r.state.is_active())
                    .collect())
            })
        })
        .await
    }
    async fn list_active(&self) -> Result<Vec<PreviewInstanceRecord>, Error> {
        self.run(|tx| Box::pin(active(tx))).await
    }
    async fn reconcile_host_reboot(
        &self,
        pod_uid: &str,
        boot_id: &str,
    ) -> Result<Vec<PreviewInstanceRecord>, Error> {
        let pod_uid = pod_uid.to_owned();
        let boot_id = boot_id.to_owned();
        self.run(move |tx| {
            Box::pin(async move {
                if pod_uid.is_empty()
                    || boot_id.is_empty()
                    || pod_uid.contains(':')
                    || boot_id.contains(':')
                {
                    return Err(Error::Invalid("Invalid preview host identity".into()));
                }
                let current_host = format!("{pod_uid}:{boot_id}");
                let candidates = active(tx).await?;
                let mut changed = Vec::new();
                // Stable key order prevents deadlocks with batch activity writes.
                for candidate in candidates {
                    if candidate.host_id.split_once(':').map(|(pod, _)| pod)
                        != Some(pod_uid.as_str())
                        || candidate.host_id == current_host
                    {
                        continue;
                    }
                    let before = current(tx, &candidate.preview_key).await?;
                    if before.instance_id != candidate.instance_id
                        || before.host_id != candidate.host_id
                        || !before.state.is_active()
                    {
                        continue;
                    }
                    let mut row = before.clone();
                    row.state = PreviewInstanceState::Stopped;
                    row.pid = None;
                    row.detail = Some(format!("Host reboot reconciled by {current_host}"));
                    row.updated_at = now();
                    save(tx, &before, &row).await?;
                    let op = operation(tx, &row.operation_id)
                        .await?
                        .ok_or_else(|| unavailable("Missing preview operation"))?;
                    finish(
                        tx,
                        &row,
                        if op.kind == "stop" {
                            "succeeded"
                        } else {
                            "failed"
                        },
                        serde_json::json!({"recovery":row.detail}),
                    )
                    .await?;
                    changed.push(row);
                }
                Ok(changed)
            })
        })
        .await
    }
}
