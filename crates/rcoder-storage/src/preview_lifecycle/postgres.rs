//! 预览权威注册表 PG 实现。
//!
//! 事务与 CAS 策略：
//! - `accept_start` / `accept_stop` 用 `SELECT ... FOR UPDATE` + 事务内端口分配，
//!   保证「受理 + 端口占用」原子成立（端口全局唯一的前提）；
//! - 其余变更全部条件更新（WHERE instance_id/operation_id/revision + state 前置），
//!   受影响行数为 0 即 CAS 失败，返回当前行供协调器判定；
//! - 存储错误原样 `Unavailable`（含 SQL 串号），绝不映射为"不存在"。
//!
//! 全部语句为字面量 SQL（sqlx SqlSafeStr 约束：动态字符串需显式审计标记）；
//! 行读取用 `SELECT *` + FromRow 按列名取值，与表结构演进解耦。
use shared_types::{
    AcceptStartInput, AcceptStartOutcome, PREVIEW_PORT_MAX, PREVIEW_PORT_MIN,
    PREVIEW_PORT_RESERVED_MAX, PREVIEW_PORT_RESERVED_MIN, PreviewInstanceRecord,
    PreviewLifecycleStore, PreviewStoreError as Error, is_preview_port,
};

use super::domain::{self, PreviewRow};

/// 活跃实例的端口占用记账查询（与部分索引同词汇）。
const SELECT_ACTIVE_PORTS: &str =
    "SELECT port FROM preview_instances WHERE state IN ('starting','ready','stopping','unknown')";

pub struct PgPreviewStore {
    pool: sqlx::PgPool,
}

impl PgPreviewStore {
    /// 独立连接（复用共享连接策略；rcoder 装配入口）。
    pub async fn connect(config: &crate::config::PostgresConfig) -> Result<Self, Error> {
        let pool = crate::pg::connection::connect_pool(config)
            .await
            .map_err(unavailable)?;
        Self::open(pool).await
    }

    /// 复用已有连接池；迁移写独立表 `_sqlx_preview_migrations`。
    pub async fn open(pool: sqlx::PgPool) -> Result<Self, Error> {
        let mut migrator = sqlx::migrate!("./migrations-preview-pg");
        migrator.table_name = "_sqlx_preview_migrations".into();
        migrator.run(&pool).await.map_err(unavailable)?;
        Ok(Self { pool })
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

fn unavailable(error: impl std::fmt::Display) -> Error {
    Error::Unavailable(error.to_string())
}

fn conflict(context: &str, current: Option<&PreviewInstanceRecord>) -> Error {
    Error::Conflict(match current {
        Some(record) => format!(
            "{context}: state={}, revision={}",
            domain::state_str(record.state),
            record.revision
        ),
        None => context.to_string(),
    })
}

async fn fetch_current(
    tx: &mut sqlx::PgConnection,
    preview_key: &str,
) -> Result<Option<PreviewInstanceRecord>, Error> {
    let row: Option<PreviewRow> =
        sqlx::query_as::<_, PreviewRow>("SELECT * FROM preview_instances WHERE preview_key=$1")
            .bind(preview_key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(unavailable)?;
    row.map(|r| r.to_record()).transpose()
}

fn allocate_port(occupied: &[u16], requested: Option<u16>) -> Result<u16, Error> {
    match requested {
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
            Ok(requested)
        }
        None => (PREVIEW_PORT_MIN..=PREVIEW_PORT_MAX)
            .filter(|p| {
                !(PREVIEW_PORT_RESERVED_MIN..=PREVIEW_PORT_RESERVED_MAX).contains(p)
                    && !occupied.contains(p)
            })
            .min()
            .ok_or_else(|| Error::Invalid("preview port pool exhausted".into())),
    }
}

#[async_trait::async_trait]
impl PreviewLifecycleStore for PgPreviewStore {
    async fn accept_start(&self, input: AcceptStartInput) -> Result<AcceptStartOutcome, Error> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;

        // 1. 锁定现行行并做前置判定（只读路径直接返回，事务无写即回滚）。
        let existing: Option<PreviewRow> = sqlx::query_as::<_, PreviewRow>(
            "SELECT * FROM preview_instances WHERE preview_key=$1 FOR UPDATE",
        )
        .bind(&input.preview_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(unavailable)?;

        let mut next_revision = 1_i64;
        if let Some(row) = &existing {
            let record = row.to_record()?;
            match record.state {
                shared_types::PreviewInstanceState::Ready => {
                    return Ok(AcceptStartOutcome::ExistingReady(record));
                }
                shared_types::PreviewInstanceState::Starting
                | shared_types::PreviewInstanceState::Stopping => {
                    return Ok(AcceptStartOutcome::Blocked(record));
                }
                shared_types::PreviewInstanceState::Unknown => {
                    let Some(evidence) = input.recover_unknown_evidence.as_deref() else {
                        return Ok(AcceptStartOutcome::Blocked(record));
                    };
                    // 证据已由协调器核实：先落 Stopped 再受理新实例（同事务）。
                    let updated = sqlx::query(
                        "UPDATE preview_instances SET state='stopped', pid=NULL, detail=$1, \
                         updated_at=now() \
                         WHERE preview_key=$2 AND instance_id=$3 AND state='unknown'",
                    )
                    .bind(evidence)
                    .bind(&input.preview_key)
                    .bind(&record.instance_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(unavailable)?;
                    if updated.rows_affected() == 0 {
                        return Err(conflict(
                            "unknown instance changed during recovery",
                            Some(&record),
                        ));
                    }
                    next_revision = record.revision + 1;
                }
                shared_types::PreviewInstanceState::Stopped
                | shared_types::PreviewInstanceState::Failed => {
                    next_revision = record.revision + 1;
                }
            }
        }

        // 2. 端口占用集合（与受理同事务，构成全局唯一分配）。
        let occupied: Vec<i32> = sqlx::query_scalar::<_, i32>(SELECT_ACTIVE_PORTS)
            .fetch_all(&mut *tx)
            .await
            .map_err(unavailable)?;
        let occupied: Vec<u16> = occupied
            .into_iter()
            .filter_map(|p| u16::try_from(p).ok())
            .collect();
        let port = allocate_port(&occupied, input.requested_port)?;

        // 3. 受理落库（starting）。
        let upserted: PreviewRow = sqlx::query_as::<_, PreviewRow>(
            "INSERT INTO preview_instances (preview_key, project_id, project_path, instance_id, \
             revision, operation_id, host_id, pod_name, pod_ip, pid, port, base_path, state, \
             last_heartbeat_at, last_activity_at, detail, updated_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,NULL,$10,NULL,'starting',NULL,now(),$11,now()) \
             ON CONFLICT (preview_key) DO UPDATE SET \
             instance_id=$4, revision=$5, operation_id=$6, host_id=$7, pod_name=$8, pod_ip=$9, \
             pid=NULL, port=$10, base_path=NULL, state='starting', last_heartbeat_at=NULL, \
             last_activity_at=now(), detail=$11, updated_at=now() \
             RETURNING *",
        )
        .bind(&input.preview_key)
        .bind(&input.project_id)
        .bind(&input.project_path)
        .bind(&input.instance_id)
        .bind(next_revision)
        .bind(&input.operation_id)
        .bind(&input.host.host_id)
        .bind(&input.host.pod_name)
        .bind(&input.host.pod_ip)
        .bind(i32::from(port))
        .bind(input.recover_unknown_evidence.as_deref())
        .fetch_one(&mut *tx)
        .await
        .map_err(unavailable)?;

        sqlx::query(
            "INSERT INTO preview_operations (operation_id, preview_key, kind, state, host_id, \
             requested_port, allocated_port, result, created_at, updated_at) \
             VALUES ($1,$2,'start','accepted',$3,$4,$5,NULL,now(),now())",
        )
        .bind(&input.operation_id)
        .bind(&input.preview_key)
        .bind(&input.host.host_id)
        .bind(input.requested_port.map(i32::from))
        .bind(i32::from(port))
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;

        tx.commit().await.map_err(unavailable)?;
        Ok(AcceptStartOutcome::Admitted(upserted.to_record()?))
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
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let row: Option<PreviewRow> = sqlx::query_as::<_, PreviewRow>(
            "UPDATE preview_instances SET state='ready', pid=$4, port=$5, base_path=$6, \
             last_heartbeat_at=now(), last_activity_at=now(), updated_at=now() \
             WHERE preview_key=$1 AND operation_id=$2 AND revision=$3 AND state='starting' \
             RETURNING *",
        )
        .bind(preview_key)
        .bind(operation_id)
        .bind(revision)
        .bind(pid)
        .bind(i32::from(port))
        .bind(base_path)
        .fetch_optional(&mut *tx)
        .await
        .map_err(unavailable)?;
        let Some(row) = row else {
            let current = fetch_current(&mut tx, preview_key).await?;
            return Err(conflict("publish_running lost the race", current.as_ref()));
        };
        sqlx::query(
            "UPDATE preview_operations SET state='succeeded', result=$3, updated_at=now() \
             WHERE operation_id=$1 AND preview_key=$2",
        )
        .bind(operation_id)
        .bind(preview_key)
        .bind(format!("pid={pid} port={port}"))
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        row.to_record()
    }

    async fn accept_stop(
        &self,
        preview_key: &str,
        operation_id: &str,
    ) -> Result<PreviewInstanceRecord, Error> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let existing: Option<PreviewRow> = sqlx::query_as::<_, PreviewRow>(
            "SELECT * FROM preview_instances WHERE preview_key=$1 FOR UPDATE",
        )
        .bind(preview_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(unavailable)?;
        let Some(row) = existing else {
            return Err(Error::Invalid(format!(
                "preview {preview_key} has no instance to stop"
            )));
        };
        let record = row.to_record()?;
        if !record.state.is_active() {
            // Stopped/Failed：无进程可停，幂等返回（协调器按"已停止"回信封）。
            return Ok(record);
        }
        let updated: PreviewRow = sqlx::query_as::<_, PreviewRow>(
            "UPDATE preview_instances SET state='stopping', operation_id=$2, \
             revision=revision+1, updated_at=now() \
             WHERE preview_key=$1 AND instance_id=$3 AND revision=$4 \
             RETURNING *",
        )
        .bind(preview_key)
        .bind(operation_id)
        .bind(&record.instance_id)
        .bind(record.revision)
        .fetch_one(&mut *tx)
        .await
        .map_err(unavailable)?;
        sqlx::query(
            "INSERT INTO preview_operations (operation_id, preview_key, kind, state, host_id, \
             requested_port, allocated_port, result, created_at, updated_at) \
             VALUES ($1,$2,'stop','accepted',$3,NULL,$4,NULL,now(),now())",
        )
        .bind(operation_id)
        .bind(preview_key)
        .bind(&record.host_id)
        .bind(record.port.map(i32::from))
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        updated.to_record()
    }

    async fn mark_stopped(
        &self,
        preview_key: &str,
        operation_id: &str,
        revision: i64,
    ) -> Result<PreviewInstanceRecord, Error> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let row: Option<PreviewRow> = sqlx::query_as::<_, PreviewRow>(
            "UPDATE preview_instances SET state='stopped', pid=NULL, updated_at=now() \
             WHERE preview_key=$1 AND operation_id=$2 AND revision=$3 AND state='stopping' \
             RETURNING *",
        )
        .bind(preview_key)
        .bind(operation_id)
        .bind(revision)
        .fetch_optional(&mut *tx)
        .await
        .map_err(unavailable)?;
        let Some(row) = row else {
            let current = fetch_current(&mut tx, preview_key).await?;
            // 并发 stop 已完成 → 幂等成功；否则才是真冲突。
            if let Some(record) = &current
                && record.state == shared_types::PreviewInstanceState::Stopped
            {
                return Ok(current.expect("checked Some"));
            }
            return Err(conflict("mark_stopped lost the race", current.as_ref()));
        };
        sqlx::query(
            "UPDATE preview_operations SET state='succeeded', result='stopped', updated_at=now() \
             WHERE operation_id=$1 AND preview_key=$2",
        )
        .bind(operation_id)
        .bind(preview_key)
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        row.to_record()
    }

    async fn mark_failed(
        &self,
        preview_key: &str,
        instance_id: &str,
        revision: i64,
        detail: &str,
    ) -> Result<PreviewInstanceRecord, Error> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let row: Option<PreviewRow> = sqlx::query_as::<_, PreviewRow>(
            "UPDATE preview_instances SET state='failed', pid=NULL, detail=$4, updated_at=now() \
             WHERE preview_key=$1 AND instance_id=$2 AND revision=$3 \
             AND state IN ('starting','ready') \
             RETURNING *",
        )
        .bind(preview_key)
        .bind(instance_id)
        .bind(revision)
        .bind(detail)
        .fetch_optional(&mut *tx)
        .await
        .map_err(unavailable)?;
        let Some(row) = row else {
            let current = fetch_current(&mut tx, preview_key).await?;
            return Err(conflict("mark_failed lost the race", current.as_ref()));
        };
        sqlx::query(
            "UPDATE preview_operations SET state='failed', result=$2, updated_at=now() \
             WHERE preview_key=$1 AND state IN ('accepted','running')",
        )
        .bind(preview_key)
        .bind(detail)
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        row.to_record()
    }

    async fn mark_unknown(
        &self,
        preview_key: &str,
        instance_id: &str,
        detail: &str,
    ) -> Result<PreviewInstanceRecord, Error> {
        let row: Option<PreviewRow> = sqlx::query_as::<_, PreviewRow>(
            "UPDATE preview_instances SET state='unknown', detail=$3, updated_at=now() \
             WHERE preview_key=$1 AND instance_id=$2 \
             AND state IN ('starting','ready','stopping','unknown') \
             RETURNING *",
        )
        .bind(preview_key)
        .bind(instance_id)
        .bind(detail)
        .fetch_optional(&self.pool)
        .await
        .map_err(unavailable)?;
        row.map(|r| r.to_record())
            .transpose()?
            .ok_or_else(|| Error::Conflict(format!("instance {instance_id} not in active state")))
    }

    async fn resolve_unknown_stopped(
        &self,
        preview_key: &str,
        instance_id: &str,
        evidence: &str,
    ) -> Result<PreviewInstanceRecord, Error> {
        let row: Option<PreviewRow> = sqlx::query_as::<_, PreviewRow>(
            "UPDATE preview_instances SET state='stopped', pid=NULL, detail=$3, updated_at=now() \
             WHERE preview_key=$1 AND instance_id=$2 AND state='unknown' \
             RETURNING *",
        )
        .bind(preview_key)
        .bind(instance_id)
        .bind(evidence)
        .fetch_optional(&self.pool)
        .await
        .map_err(unavailable)?;
        row.map(|r| r.to_record())
            .transpose()?
            .ok_or_else(|| Error::Conflict(format!("instance {instance_id} is not unknown")))
    }

    async fn refresh_heartbeat(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> Result<Option<PreviewInstanceRecord>, Error> {
        let row: Option<PreviewRow> = sqlx::query_as::<_, PreviewRow>(
            "UPDATE preview_instances SET last_heartbeat_at=now() \
             WHERE preview_key=$1 AND instance_id=$2 AND state IN ('ready','unknown') \
             RETURNING *",
        )
        .bind(preview_key)
        .bind(instance_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(unavailable)?;
        row.map(|r| r.to_record()).transpose()
    }

    async fn flush_activity(
        &self,
        entries: &[shared_types::ActivityFlushEntry],
    ) -> Result<usize, Error> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        let mut updated = 0_usize;
        for entry in entries {
            let rows = sqlx::query(
                "UPDATE preview_instances SET last_activity_at=GREATEST(last_activity_at,$3) \
                 WHERE preview_key=$1 AND instance_id=$2 \
                 AND state IN ('starting','ready','stopping','unknown')",
            )
            .bind(&entry.preview_key)
            .bind(&entry.instance_id)
            .bind(entry.at)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
            updated += usize::try_from(rows.rows_affected()).unwrap_or(0);
        }
        tx.commit().await.map_err(unavailable)?;
        Ok(updated)
    }

    async fn get(&self, preview_key: &str) -> Result<Option<PreviewInstanceRecord>, Error> {
        let row: Option<PreviewRow> =
            sqlx::query_as::<_, PreviewRow>("SELECT * FROM preview_instances WHERE preview_key=$1")
                .bind(preview_key)
                .fetch_optional(&self.pool)
                .await
                .map_err(unavailable)?;
        row.map(|r| r.to_record()).transpose()
    }

    async fn find_active_by_port(&self, port: u16) -> Result<Option<PreviewInstanceRecord>, Error> {
        let row: Option<PreviewRow> = sqlx::query_as::<_, PreviewRow>(
            "SELECT * FROM preview_instances \
             WHERE port=$1 AND state IN ('starting','ready','stopping','unknown') \
             ORDER BY revision DESC LIMIT 1",
        )
        .bind(i32::from(port))
        .fetch_optional(&self.pool)
        .await
        .map_err(unavailable)?;
        row.map(|r| r.to_record()).transpose()
    }

    async fn active_ports(&self) -> Result<Vec<u16>, Error> {
        let ports: Vec<i32> = sqlx::query_scalar::<_, i32>(SELECT_ACTIVE_PORTS)
            .fetch_all(&self.pool)
            .await
            .map_err(unavailable)?;
        Ok(ports
            .into_iter()
            .filter_map(|p| u16::try_from(p).ok())
            .collect())
    }

    async fn list_by_host(
        &self,
        host_id: &str,
        active_only: bool,
    ) -> Result<Vec<PreviewInstanceRecord>, Error> {
        let sql = if active_only {
            "SELECT * FROM preview_instances WHERE host_id=$1 \
             AND state IN ('starting','ready','stopping','unknown') ORDER BY preview_key"
        } else {
            "SELECT * FROM preview_instances WHERE host_id=$1 ORDER BY preview_key"
        };
        let rows: Vec<PreviewRow> = sqlx::query_as::<_, PreviewRow>(sql)
            .bind(host_id)
            .fetch_all(&self.pool)
            .await
            .map_err(unavailable)?;
        rows.into_iter().map(|r| r.to_record()).collect()
    }

    async fn list_active(&self) -> Result<Vec<PreviewInstanceRecord>, Error> {
        let rows: Vec<PreviewRow> = sqlx::query_as::<_, PreviewRow>(
            "SELECT * FROM preview_instances \
             WHERE state IN ('starting','ready','stopping','unknown') ORDER BY preview_key",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(unavailable)?;
        rows.into_iter().map(|r| r.to_record()).collect()
    }

    async fn reconcile_host_reboot(
        &self,
        pod_uid: &str,
        boot_id: &str,
    ) -> Result<Vec<PreviewInstanceRecord>, Error> {
        let current_host = format!("{pod_uid}:{boot_id}");
        // pod_uid 为 UUID（无 LIKE 通配字符），前缀经参数绑定安全匹配。
        let prefix = format!("{pod_uid}:%");
        let rows: Vec<PreviewRow> = sqlx::query_as::<_, PreviewRow>(
            "UPDATE preview_instances SET state='stopped', pid=NULL, detail=$1, updated_at=now() \
             WHERE host_id LIKE $2 AND host_id <> $3 \
             AND state IN ('starting','ready','stopping','unknown') \
             RETURNING *",
        )
        .bind(format!("host reboot reconciled by {current_host}"))
        .bind(&prefix)
        .bind(&current_host)
        .fetch_all(&self.pool)
        .await
        .map_err(unavailable)?;
        rows.into_iter().map(|r| r.to_record()).collect()
    }
}
