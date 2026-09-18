//! Turso 本地控制存储（specs/userapp-turso-local-storage plan §3）。
//!
//! 架构：专用数据库线程 + 有界队列。线程内运行 current-thread tokio
//! runtime 并独占 Connection；队列单位是**完整 store 方法**（Boxed future
//! 工厂），线程内顺序完成全部语句 + commit/rollback 再处理下一任务——
//! 不同请求的事务不会在同一连接上交错。调用方丢弃 future 不中止已开始
//! 的事务（oneshot 发送失败即丢弃回包，事务照常完成——可能已提交但
//! 回包丢失，调用方按原 request_id/operation_id 幂等查询）。

mod exec;
mod migrations;
mod ops;

use futures::future::BoxFuture;
use shared_types::{UserAppLifecycleRecord, UserAppOperationRecord, UserAppStoreError as Error};
use tokio::sync::{mpsc, oneshot};

use crate::userapp_lifecycle::storage;

/// 队列深度（有界——满时明确报错，不静默丢写）。
const QUEUE_DEPTH: usize = 256;

/// 队列满/关机时的明确错误（plan §3 取消与 deadline）。
fn unavailable(reason: &str) -> Error {
    Error::Storage(anyhow::anyhow!(
        "userapp store worker unavailable: {reason}"
    ))
}

/// 一个排队任务：对独占连接的完整方法体 + 回包通道。
type TaskBody = Box<
    dyn for<'c> FnOnce(&'c mut turso::Connection) -> BoxFuture<'c, Result<TaskOut, Error>> + Send,
>;

struct Task {
    body: TaskBody,
    reply: oneshot::Sender<Result<TaskOut, Error>>,
}

/// 类型擦除的返回值（worker 侧完成执行；channel 侧由泛型包装解包）。
struct TaskOut(Box<dyn std::any::Any + Send>);

/// 业务门面：句柄轻量可克隆（mpsc sender）；Connection 只存在于 worker。
pub struct TursoUserAppStore {
    tx: mpsc::Sender<Task>,
    /// 关机协调句柄（装配层持有；Drop 只作兜底）。
    worker: std::sync::Mutex<Option<WorkerHandle>>,
    /// 进程独占目录锁（连接随 worker 生命周期；锁同样随本结构体持有——
    /// `let _ = lock` 会在语句末 drop，第二实例将不被拒绝）。
    _instance_lock: Option<std::fs::File>,
}

struct WorkerHandle {
    shutdown: tokio::sync::watch::Sender<bool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl TursoUserAppStore {
    /// 独占打开：目录锁 → worker/连接 → PRAGMA 校验 → 迁移 → 重启隔离。
    /// 整个期间锁归 worker 生命周期持有（plan §3 启动顺序）。
    pub async fn open_exclusive(path: &std::path::Path) -> Result<Self, Error> {
        let path = path.to_path_buf();
        // 独占目录校验复用后端中性实现（与原 SQLite 相同的别名/远程 fs 保护）
        let (db_path, lock) =
            tokio::task::spawn_blocking(move || super::exclusive_directory::acquire(&path))
                .await
                .map_err(storage)??;
        let (mut store, handle) = Self::spawn_worker(&db_path).await?;
        // 重启隔离（在 worker 侧、对外 ready 前执行）
        let _isolated: () = store
            .run(|conn: &mut turso::Connection| {
                Box::pin(async move {
                    restart::quarantine(conn)
                        .await
                        .map(|r| TaskOut(Box::new(r)))
                })
            })
            .await?;
        store._instance_lock = Some(lock);
        *store.worker.lock().map_err(|_| unavailable("poisoned"))? = Some(handle);
        Ok(store)
    }
    /// 内部：起 worker 线程并就绪（连接+PRAGMA+迁移已完成）。
    async fn spawn_worker(db_path: &std::path::Path) -> Result<(Self, WorkerHandle), Error> {
        let (ready_tx, ready_rx) = oneshot::channel::<
            Result<(mpsc::Sender<Task>, tokio::sync::watch::Sender<bool>), Error>,
        >();
        let db_path = db_path.to_path_buf();
        let join = std::thread::Builder::new()
            .name("userapp-turso-store".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build turso store runtime");
                runtime.block_on(async move {
                    let (tx, mut rx) = mpsc::channel::<Task>(QUEUE_DEPTH);
                    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
                    // 连接 + PRAGMA + 迁移（worker 内完成——连接不跨线程）
                    let mut conn = match init_connection(&db_path).await {
                        Ok(conn) => {
                            drop(ready_tx.send(Ok((tx.clone(), shutdown_tx))));
                            conn
                        }
                        Err(error) => {
                            drop(ready_tx.send(Err(error)));
                            return;
                        }
                    };
                    // 主循环：顺序处理完整方法；shutdown 信号先排空已接收任务
                    loop {
                        tokio::select! {
                            biased;
                            _ = shutdown_rx.changed() => {
                                if *shutdown_rx.borrow() {
                                    rx.close();
                                    while let Some(task) = rx.recv().await {
                                        execute_task(&mut conn, task).await;
                                    }
                                    break;
                                }
                            }
                            task = rx.recv() => {
                                match task {
                                    Some(task) => execute_task(&mut conn, task).await,
                                    None => break,
                                }
                            }
                        }
                    }
                    // 连接随线程结束销毁（无跨线程 Send 声明需求）
                });
            })
            .map_err(|e| storage(anyhow::anyhow!("spawn turso store worker: {e}")))?;
        match ready_rx.await {
            Ok(Ok((tx, shutdown))) => Ok((
                Self {
                    tx,
                    worker: std::sync::Mutex::new(None),
                    _instance_lock: None,
                },
                WorkerHandle {
                    shutdown,
                    join: Some(join),
                },
            )),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(unavailable("worker died during init")),
        }
    }
    /// 在 worker 上执行一个完整方法体。
    ///
    /// 入队前取消安全（try_send/send 满或关机 → 未执行 + 明确错误）；
    /// 入队后调用方丢弃 future 不中止事务（oneshot drop → 结果丢弃）。
    async fn run<R: Send + 'static>(
        &self,
        body: impl for<'c> FnOnce(&'c mut turso::Connection) -> BoxFuture<'c, Result<TaskOut, Error>>
        + Send
        + 'static,
    ) -> Result<R, Error> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Task {
                body: Box::new(body),
                reply: reply_tx,
            })
            .await
            .map_err(|_| unavailable("worker stopped"))?;
        match reply_rx.await {
            Ok(result) => result.and_then(|out| {
                out.0
                    .downcast::<R>()
                    .map_err(|_| Error::InvalidOperation("store result type mismatch".into()))
                    .map(|boxed| *boxed)
            }),
            Err(_) => Err(unavailable("worker dropped reply without executing")),
        }
    }
    /// 显式关机（装配层）：停生产者 → 排空队列 → 关连接 → join。
    /// 重复调用安全（幂等）。
    pub async fn shutdown(&self) -> Result<(), Error> {
        let handle = {
            self.worker
                .lock()
                .map_err(|_| unavailable("poisoned"))?
                .take()
        };
        let Some(handle) = handle else {
            return Ok(()); // 已关机（幂等）
        };
        handle
            .shutdown
            .send(true)
            .map_err(|_| unavailable("shutdown channel closed"))?;
        // join 在阻塞线程上完成（不在 async 上下文同步阻塞——spawn_blocking）
        let join = handle.join;
        tokio::task::spawn_blocking(move || {
            if let Some(join) = join {
                drop(join.join());
            }
        })
        .await
        .map_err(|e| storage(anyhow::anyhow!("join worker: {e}")))?;
        Ok(())
    }
}

async fn execute_task(conn: &mut turso::Connection, task: Task) {
    let body = task.body;
    let reply = task.reply;
    let result = body(conn).await;
    // 回包丢失（调用方取消）→ 结果丢弃；事务已在 body 内 commit 或由
    // dangling_tx 机制回滚（见 exec.rs 文档）
    drop(reply.send(result));
}

/// 离线快照（e2e 工具专用，非业务接口）：持实例目录锁后以 Turso 引擎直读
/// 生命周期与操作记录。不执行迁移、PRAGMA 写或重启隔离——目录锁互斥保证
/// 观察时主实例已停止（tasks.md T4：禁止 SQLite 引擎读取 Turso 活库，
/// 观察器必须持锁且不执行启动隔离）。
pub async fn offline_snapshot(path: &std::path::Path) -> Result<(Vec<String>, Vec<String>), Error> {
    let owned = path.to_path_buf();
    // acquire 内含别名/本地文件系统校验——与主实例同一套保护
    let (db_path, _lock) =
        tokio::task::spawn_blocking(move || super::exclusive_directory::acquire(&owned))
            .await
            .map_err(storage)??;
    let db = turso::Builder::new_local(db_path.to_string_lossy().as_ref())
        .build()
        .await
        .map_err(storage)?;
    let conn = db.connect().map_err(storage)?;
    let lifecycles = ex_read_all(&conn, "SELECT record FROM userapp_lifecycles").await?;
    let operations = ex_read_all(&conn, "SELECT record FROM userapp_operations").await?;
    Ok((lifecycles, operations))
}

async fn ex_read_all(conn: &turso::Connection, sql: &str) -> Result<Vec<String>, Error> {
    let mut rows = conn.query(sql, ()).await.map_err(storage)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(storage)? {
        match row.get_value(0).map_err(storage)? {
            turso::Value::Text(record) => out.push(record),
            other => {
                return Err(Error::InvalidOperation(format!(
                    "offline snapshot expected text record, got {other:?}"
                )));
            }
        }
    }
    Ok(out)
}

/// 连接初始化：builder → connect → WAL/FULL/FK 配置+读回校验 → 迁移。
/// 在 worker 线程的 runtime 内 await（plan §3 启动顺序）。
async fn init_connection(db_path: &std::path::Path) -> Result<turso::Connection, Error> {
    let db = turso::Builder::new_local(db_path.to_string_lossy().as_ref())
        .build()
        .await
        .map_err(storage)?;
    let conn = db.connect().map_err(storage)?;
    // PRAGMA 设置：journal_mode 赋值**返回结果行**（新值），必须走 query
    // API 消费行（plan §4：读回 PRAGMA 不当无结果 execute——execute_batch
    // 遇行报 "unexpected row"）。synchronous/foreign_keys 赋值无返回行。
    let mut rows = conn
        .query("PRAGMA journal_mode=wal", ())
        .await
        .map_err(storage)?;
    while rows.next().await.map_err(storage)?.is_some() {}
    drop(rows);
    conn.execute_batch("PRAGMA synchronous=full; PRAGMA foreign_keys=on;")
        .await
        .map_err(storage)?;
    verify_pragmas(&conn).await?;
    // 迁移器在恢复扫描与对外服务前运行
    migrations::run(&conn).await.map_err(storage)?;
    Ok(conn)
}

async fn verify_pragmas(conn: &turso::Connection) -> Result<(), Error> {
    async fn check(conn: &turso::Connection, name: &str, expected: &str) -> Result<(), Error> {
        let mut rows = conn
            .query(&format!("PRAGMA {name}"), ())
            .await
            .map_err(storage)?;
        match rows.next().await.map_err(storage)? {
            Some(row) => {
                let value = row.get_value(0).map_err(storage)?;
                let actual = match value {
                    turso::Value::Text(s) => s,
                    turso::Value::Integer(i) => i.to_string(),
                    other => format!("{other:?}"),
                };
                if !actual.eq_ignore_ascii_case(expected) {
                    return Err(Error::InvalidOperation(format!(
                        "durability pragma {name}={actual}, expected {expected}"
                    )));
                }
                Ok(())
            }
            None => Err(Error::InvalidOperation(format!(
                "PRAGMA {name} returned no row"
            ))),
        }
    }
    check(conn, "journal_mode", "wal").await?;
    check(conn, "foreign_keys", "1").await?;
    Ok(())
}

/// 重启隔离（Turso 版 restart::quarantine——同业务规则：不确定转
/// RecoveryRequired，不清租约、不重放写）。
mod restart {
    use super::exec::{begin, text};
    use crate::userapp_lifecycle::storage;
    use shared_types::{
        UserAppOperationRecord, UserAppOperationState as State, UserAppStoreError as Error,
    };

    pub(super) async fn quarantine(conn: &mut turso::Connection) -> Result<(), Error> {
        let mut tx = begin(conn).await?;
        let records = tx
            .all_string(
                "SELECT record FROM userapp_operations WHERE terminal=0 \
                 AND json_extract(record, '$.state') IN ('Running','WaitingRetry')",
                vec![],
            )
            .await?;
        let count = records.len();
        for original in records {
            let mut record: UserAppOperationRecord =
                serde_json::from_str(&original).map_err(storage)?;
            record.state = State::RecoveryRequired;
            record.revision = record.revision.checked_add(1).ok_or_else(|| {
                Error::InvalidOperation("Operation revision exhausted during restart".into())
            })?;
            record.error_code = Some(shared_types::error_codes::ERR_BACKEND_ERROR.into());
            record.error_message = Some(
                "Previous local executor stopped; remote outcome requires verification".into(),
            );
            let encoded = serde_json::to_string(&record).map_err(storage)?;
            let updated = tx
                .exec(
                    "UPDATE userapp_operations SET record=?1 WHERE operation_id=?2 AND record=?3 AND terminal=0",
                    vec![text(encoded), text(&record.operation_id), text(original)],
                )
                .await?;
            if updated != 1 {
                return Err(Error::VersionConflict);
            }
        }
        tx.commit().await?;
        if count != 0 {
            tracing::warn!(
                operations = count,
                "Interrupted local operations require remote outcome verification"
            );
        }
        Ok(())
    }
}

// ── trait 实现（克隆参数 → worker 队列 → 解包） ────────────────────────

/// 参数所有权化：&str/引用参数 clone 后仍是引用（闭包跨 channel 需
/// 'static）——统一 to_owned（&str → String；&T: Clone → T）。
macro_rules! clone_params {
    ($($v:ident),* $(,)?) => { $(let $v = $v.to_owned();)* };
}

#[async_trait::async_trait]
impl shared_types::UserAppLifecycleStore for TursoUserAppStore {
    async fn get_resource_binding(
        &self,
        service_type: &shared_types::ServiceType,
        physical_uid: &str,
    ) -> Result<Option<shared_types::UserAppResourceBinding>, Error> {
        clone_params!(service_type, physical_uid);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::get_resource_binding(conn, &service_type, &physical_uid)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn commit_resource_binding(
        &self,
        binding: &shared_types::UserAppResourceBinding,
        progress: &shared_types::UserAppOperationProgress,
    ) -> Result<UserAppOperationRecord, Error> {
        clone_params!(binding, progress);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::commit_resource_binding(conn, &binding, &progress)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn list_control_snapshots(
        &self,
        after_app_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<shared_types::UserAppControlSnapshot>, Error> {
        let after = after_app_id.map(str::to_string);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::list_control_snapshots(conn, after.as_deref(), limit)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn ensure_identity(&self, app_id: &str) -> Result<UserAppLifecycleRecord, Error> {
        clone_params!(app_id);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::ensure_identity(conn, &app_id)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn get_application(&self, app_id: &str) -> Result<Option<UserAppLifecycleRecord>, Error> {
        clone_params!(app_id);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::get_application(conn, &app_id)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn list_applications(
        &self,
        after_app_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<UserAppLifecycleRecord>, Error> {
        let after = after_app_id.map(str::to_string);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::list_applications(conn, after.as_deref(), limit)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn import_application(
        &self,
        legacy: &shared_types::AppMetadataRecord,
    ) -> Result<UserAppLifecycleRecord, Error> {
        clone_params!(legacy);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::import_application(conn, &legacy)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn patch_metadata(
        &self,
        patch: &shared_types::UserAppMetadataPatch,
    ) -> Result<UserAppLifecycleRecord, Error> {
        clone_params!(patch);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::patch_metadata(conn, &patch)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn admit(
        &self,
        request: &shared_types::UserAppAdmission,
    ) -> Result<shared_types::UserAppAdmissionOutcome, Error> {
        self.admit_with_input(request, None).await
    }

    async fn admit_with_input(
        &self,
        request: &shared_types::UserAppAdmission,
        input: Option<&shared_types::UserAppExecutionInput>,
    ) -> Result<shared_types::UserAppAdmissionOutcome, Error> {
        clone_params!(request);
        let input = input.cloned();
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::admit_with_input(conn, &request, input.as_ref())
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn read_execution_input(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> Result<shared_types::UserAppExecutionInput, Error> {
        clone_params!(context);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::read_execution_input(conn, &context)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn bind_operation_deadline(
        &self,
        app_id: &str,
        operation_id: &str,
        lifecycle_id: &str,
        deadline_epoch_ms: i64,
    ) -> Result<i64, Error> {
        clone_params!(app_id, operation_id, lifecycle_id);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::bind_operation_deadline(
                    conn,
                    &app_id,
                    &operation_id,
                    &lifecycle_id,
                    deadline_epoch_ms,
                )
                .await
                .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn operation_deadline(
        &self,
        app_id: &str,
        operation_id: &str,
    ) -> Result<Option<i64>, Error> {
        clone_params!(app_id, operation_id);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::operation_deadline(conn, &app_id, &operation_id)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn bind_operation_lease(
        &self,
        context: &shared_types::UserAppExecutionContext,
        receipt: &shared_types::UserAppOperationLeaseReceipt,
    ) -> Result<(), Error> {
        clone_params!(context, receipt);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::bind_operation_lease(conn, &context, &receipt)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn get_operation_lease(
        &self,
        app_id: &str,
        operation_id: &str,
    ) -> Result<Option<shared_types::UserAppOperationLeaseBinding>, Error> {
        clone_params!(app_id, operation_id);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::get_operation_lease(conn, &app_id, &operation_id)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn terminal_operation_leases(
        &self,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<shared_types::UserAppOperationLeaseBinding>, Error> {
        let after = after.map(str::to_string);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::terminal_operation_leases(conn, after.as_deref(), limit)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn forget_operation_lease(
        &self,
        binding: &shared_types::UserAppOperationLeaseBinding,
    ) -> Result<(), Error> {
        clone_params!(binding);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::forget_operation_lease(conn, &binding)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn reserve_completed_operation(
        &self,
        snapshot: &UserAppOperationRecord,
    ) -> Result<UserAppOperationRecord, Error> {
        clone_params!(snapshot);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::reserve_completed_operation(conn, &snapshot)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn advance(
        &self,
        progress: &shared_types::UserAppOperationProgress,
    ) -> Result<UserAppOperationRecord, Error> {
        clone_params!(progress);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::advance(conn, &progress)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn get_operation_by_request(
        &self,
        app_id: &str,
        request_id: &str,
    ) -> Result<Option<UserAppOperationRecord>, Error> {
        clone_params!(app_id, request_id);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::get_operation_by_request(conn, &app_id, &request_id)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn get_operation(
        &self,
        app_id: &str,
        operation_id: &str,
    ) -> Result<Option<UserAppOperationRecord>, Error> {
        clone_params!(app_id, operation_id);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::get_operation(conn, &app_id, &operation_id)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn unfinished_operations(
        &self,
        after_operation_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<UserAppOperationRecord>, Error> {
        let after = after_operation_id.map(str::to_string);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::unfinished_operations(conn, after.as_deref(), limit)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }

    async fn recreate(
        &self,
        app_id: &str,
        expected_lifecycle_id: &str,
        request_id: &str,
    ) -> Result<UserAppLifecycleRecord, Error> {
        clone_params!(app_id, expected_lifecycle_id, request_id);
        self.run(move |conn: &mut turso::Connection| {
            Box::pin(async move {
                ops::recreate(conn, &app_id, &expected_lifecycle_id, &request_id)
                    .await
                    .map(|r| TaskOut(Box::new(r)))
            })
        })
        .await
    }
}

impl Drop for TursoUserAppStore {
    fn drop(&mut self) {
        // Drop 只作兜底（不构成 flush 完成证据）：信号关机，不 join
        // （异步上下文不能同步阻塞 join）。
        if let Ok(mut guard) = self.worker.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.shutdown.send(true);
        }
    }
}

/// 关机控制（trait-design §6）：与业务门面同一 Arc，装配层单独持有
/// `Arc<dyn UserAppStoreControl>`；停生产者 → 排空队列 → 关连接 → join
/// 线程 → 释放目录锁（锁随结构体 Drop 释放）。
#[async_trait::async_trait]
impl crate::userapp_lifecycle::control::UserAppStoreControl for TursoUserAppStore {
    async fn shutdown(&self) -> Result<(), Error> {
        TursoUserAppStore::shutdown(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::UserAppLifecycleStore as _;

    /// 冒烟：worker + 迁移 + 基本 identity/admit/advance 链。
    #[tokio::test]
    async fn turso_store_basic_lifecycle() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("userapp.turso.db");
        let path = std::path::absolute(&path).expect("abs");
        let store = TursoUserAppStore::open_exclusive(&path)
            .await
            .expect("open");

        let app = store.ensure_identity("app-smoke").await.expect("identity");
        assert_eq!(app.app_id, "app-smoke");

        let fetched = store.get_application("app-smoke").await.expect("get");
        assert_eq!(fetched.expect("present").lifecycle_id, app.lifecycle_id);

        let missing = store.get_application("no-such").await.expect("get missing");
        assert!(missing.is_none(), "无行 = None（存储失败 ≠ 不存在）");

        // admit 一个 Stop 操作（Dev scope）
        let outcome = store
            .admit(&shared_types::UserAppAdmission {
                app_id: "app-smoke".into(),
                lifecycle_id: None,
                operation_id: "op-stop-1".into(),
                request_id: Some("req-1".into()),
                request_fingerprint: "a".repeat(64),
                kind: shared_types::UserAppOperationKind::Stop,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            })
            .await
            .expect("admit");
        match outcome {
            shared_types::UserAppAdmissionOutcome::Accepted(op) => {
                assert_eq!(op.operation_id, "op-stop-1");
            }
            other => panic!("expected Accepted, got {other:?}"),
        }

        // 幂等重放：同 request_id → Existing
        let replay = store
            .admit(&shared_types::UserAppAdmission {
                app_id: "app-smoke".into(),
                lifecycle_id: None,
                operation_id: "op-stop-1".into(),
                request_id: Some("req-1".into()),
                request_fingerprint: "a".repeat(64),
                kind: shared_types::UserAppOperationKind::Stop,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            })
            .await
            .expect("replay");
        match replay {
            shared_types::UserAppAdmissionOutcome::Existing(op) => {
                assert_eq!(op.operation_id, "op-stop-1");
            }
            other => panic!("expected Existing, got {other:?}"),
        }

        store.shutdown().await.expect("shutdown");
    }

    /// 双进程目录排他：第二个实例被拒（不能先写库再发现锁冲突）。
    #[tokio::test]
    async fn turso_store_exclusive_directory_rejects_second_instance() {
        let dir = tempfile::tempdir().expect("dir");
        let path = std::path::absolute(dir.path().join("userapp.turso.db")).expect("abs");
        let _first = TursoUserAppStore::open_exclusive(&path)
            .await
            .expect("first");
        let second = TursoUserAppStore::open_exclusive(&path).await;
        assert!(
            second.is_err(),
            "second instance must be rejected by directory lock"
        );
    }

    /// 损坏迁移拒绝：篡改版本表校验和 → 启动失败。
    #[tokio::test]
    async fn turso_store_rejects_checksum_mismatch() {
        let dir = tempfile::tempdir().expect("dir");
        let path = std::path::absolute(dir.path().join("userapp.turso.db")).expect("abs");
        let store = TursoUserAppStore::open_exclusive(&path)
            .await
            .expect("open");
        store.shutdown().await.expect("shutdown initial");
        drop(store); // 结构体持有目录锁——释放后才能以同版本引擎观察/重开
        // 篡改版本记录校验和（直接 Turso 引擎打开同版本文件——主进程已
        // 停止并持锁释放后）
        let db = turso::Builder::new_local(path.to_string_lossy().as_ref())
            .build()
            .await
            .expect("reopen");
        let conn = db.connect().expect("connect");
        conn.execute(
            "UPDATE _turso_userapp_migrations SET checksum='tampered' WHERE version=1",
            (),
        )
        .await
        .expect("tamper");
        drop(conn);
        drop(db);
        let result = TursoUserAppStore::open_exclusive(&path).await;
        let Err(error) = result else {
            panic!("checksum mismatch must block startup")
        };
        let message = format!("{error:#}");
        assert!(
            message.contains("checksum mismatch")
                || message.contains("modified after being applied"),
            "diagnostic: {message}"
        );
    }

    /// 原子性反例（对应 sqlite 时代的 trigger 注入）：事务中途存储失败 →
    /// 整个受理回滚。注入方式 = 经 worker 自己的连接预插一行占用目标
    /// operation_id（同引擎同连接，无跨引擎锁问题），使 admit 的事务内
    /// INSERT 在 lifecycle 写入之后撞 PRIMARY KEY。
    #[tokio::test]
    async fn turso_failure_midway_rolls_back_entire_admission() {
        let dir = tempfile::tempdir().expect("dir");
        let path = std::path::absolute(dir.path().join("rollback.turso.db")).expect("abs");
        let store = TursoUserAppStore::open_exclusive(&path)
            .await
            .expect("open");
        let app_id = "atomic-failure".to_string();
        let op_id = "atomic-failure-operation".to_string();

        let inject: Result<(), Error> = store
            .run(|conn: &mut turso::Connection| {
                Box::pin(async move {
                    use super::exec::text;
                    conn.execute(
                        "INSERT INTO userapp_lifecycles(app_id,record) VALUES(?1,'{\"stub\":true}')",
                        vec![text("atomic-carrier")],
                    )
                    .await
                    .map_err(storage)?;
                    conn.execute(
                        "INSERT INTO userapp_operations(operation_id,app_id,request_id,terminal,record) \
                         VALUES(?1,'atomic-carrier',NULL,0,'{\"stub\":true}')",
                        vec![text("atomic-failure-operation")],
                    )
                    .await
                    .map_err(storage)?;
                    Ok(TaskOut(Box::new(())))
                })
            })
            .await;
        inject.expect("seed collision carrier");

        let mut req = shared_types::UserAppAdmission {
            runtime_policy_on_success: None,
            command: None,
            metadata: None,
            app_id: app_id.clone(),
            lifecycle_id: None,
            operation_id: op_id.clone(),
            request_id: Some("atomic-failure-request".into()),
            request_fingerprint: "a".repeat(64),
            kind: shared_types::UserAppOperationKind::Update,
        };
        req.app_id = app_id.clone();
        assert!(
            matches!(store.admit(&req).await, Err(Error::Storage(_))),
            "in-transaction PK collision must surface as a storage failure"
        );
        assert!(
            store.get_application(&app_id).await.unwrap().is_none(),
            "fresh lifecycle insert must roll back with the failed admission"
        );
        assert!(
            store
                .get_operation(&app_id, &op_id)
                .await
                .unwrap()
                .is_none(),
            "no operation row may survive for this app"
        );

        // 清除碰撞源后重试成功（连接未被失败事务毒化）
        let cleanup: Result<(), Error> = store
            .run(|conn: &mut turso::Connection| {
                Box::pin(async move {
                    conn.execute(
                        "DELETE FROM userapp_operations WHERE app_id='atomic-carrier'",
                        (),
                    )
                    .await
                    .map_err(storage)?;
                    conn.execute(
                        "DELETE FROM userapp_lifecycles WHERE app_id='atomic-carrier'",
                        (),
                    )
                    .await
                    .map_err(storage)?;
                    Ok(TaskOut(Box::new(())))
                })
            })
            .await;
        cleanup.expect("remove carrier");
        assert!(matches!(
            store.admit(&req).await.expect("retry admission"),
            shared_types::UserAppAdmissionOutcome::Accepted(_)
        ));
        store.shutdown().await.expect("shutdown");
    }

    /// 损坏数据反例：lifecycle 槽位指向不存在的操作 → 控制面快照必须
    /// 报 InvalidOperation，不得把断链当成正常空闲状态展示。
    #[tokio::test]
    async fn turso_control_snapshot_rejects_broken_operation_link() {
        let dir = tempfile::tempdir().expect("dir");
        let path = std::path::absolute(dir.path().join("snap.turso.db")).expect("abs");
        let store = TursoUserAppStore::open_exclusive(&path)
            .await
            .expect("open");
        let mut identity = store
            .ensure_identity("snapshot-corrupt")
            .await
            .expect("identity");
        identity.active_operations.prod = Some("missing-operation".into());
        let corrupt = serde_json::to_string(&identity).expect("record");
        let inject: Result<(), Error> = store
            .run(|conn: &mut turso::Connection| {
                Box::pin(async move {
                    use super::exec::text;
                    conn.execute(
                        "UPDATE userapp_lifecycles SET record=?2 WHERE app_id=?1",
                        vec![text("snapshot-corrupt"), text(corrupt)],
                    )
                    .await
                    .map_err(storage)?;
                    Ok(TaskOut(Box::new(())))
                })
            })
            .await;
        inject.expect("inject broken link");
        assert!(
            matches!(
                store.list_control_snapshots(None, 128).await,
                Err(Error::InvalidOperation(_))
            ),
            "missing operation cannot appear as normal idle state"
        );
        store.shutdown().await.expect("shutdown");
    }

    /// 取消与遗留事务反例（worker 架构）：
    /// ① 调用方在 body 执行中丢弃 future —— 任务已在队列，body 照常完成，
    ///    结果丢弃，队列不卡死；
    /// ② body 内开事务写入后返回错误（未终结）—— dangling 事务必须在
    ///    下一条语句前回滚，半写状态不可见；
    /// ③ 之后受理照常成功。
    #[tokio::test]
    async fn turso_cancelled_caller_and_dangling_transaction_do_not_leak() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let dir = tempfile::tempdir().expect("dir");
            let path = std::path::absolute(dir.path().join("cancel.turso.db")).expect("abs");
            let store = TursoUserAppStore::open_exclusive(&path).await.expect("open");

            let slow = store.run::<String>(|_conn: &mut turso::Connection| {
                Box::pin(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    Ok(TaskOut(Box::new("slow-done".to_string())))
                })
            });
            // 任务已入队（body 睡眠中），调用方超时取消 —— 只取消等待，不中止 worker
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), slow)
                    .await
                    .is_err()
            );

            let dangling: Result<String, Error> = store
                .run(|conn: &mut turso::Connection| {
                    Box::pin(async move {
                        use super::exec::{begin, text};
                        let mut tx = begin(conn).await?;
                        tx.exec(
                            "INSERT INTO userapp_lifecycles(app_id,record) VALUES(?1,'{\"stub\":true}')",
                            vec![text("dangling-app")],
                        )
                        .await?;
                        Err(Error::InvalidOperation("injected dangling transaction".into()))
                    })
                })
                .await;
            assert!(dangling.is_err());
            assert!(
                store.get_application("dangling-app").await.unwrap().is_none(),
                "dangling transaction must roll back before the next statement"
            );

            let request = shared_types::UserAppAdmission {
                runtime_policy_on_success: None,
                command: None,
                metadata: None,
                app_id: "contract-app".into(),
                lifecycle_id: None,
                operation_id: "cancelled".into(),
                request_id: Some("cancelled".into()),
                request_fingerprint: "a".repeat(64),
                kind: shared_types::UserAppOperationKind::EnsureBuilder,
            };
            assert!(
                store.get_application("contract-app").await.unwrap().is_none(),
                "cancelled caller must leave no application state behind"
            );
            assert!(matches!(
                store.admit(&request).await.expect("admission after cancel"),
                shared_types::UserAppAdmissionOutcome::Accepted(_)
            ));
            store.shutdown().await.expect("shutdown");
        })
        .await
        .expect("cancelled caller and dangling transaction must not leak");
    }

    /// 隔离只针对 Running/WaitingRetry：转 RecoveryRequired（revision+1 +
    /// ERR_BACKEND_ERROR）；Pending 与终态不动；租约记录不清（不 steal
    /// lease、不重放写）。
    #[tokio::test]
    async fn turso_restart_quarantines_only_interrupted_claims_without_replaying() {
        use shared_types::UserAppLifecycleStore as _;
        let dir = tempfile::tempdir().expect("dir");
        let path = std::path::absolute(dir.path().join("quarantine.turso.db")).expect("abs");
        let store = TursoUserAppStore::open_exclusive(&path)
            .await
            .expect("open");
        let progress = |op: &UserAppOperationRecord, state: shared_types::UserAppOperationState| {
            shared_types::UserAppOperationProgress {
                app_id: op.app_id.clone(),
                operation_id: op.operation_id.clone(),
                lifecycle_id: op.lifecycle_id.clone(),
                expected_revision: op.revision,
                executor_id: "worker-A".into(),
                state,
                step: "quarantine-probe".into(),
                checkpoint: serde_json::Value::Null,
                error_code: None,
                error_message: None,
            }
        };
        async fn admit(store: &TursoUserAppStore, app: &str, op: &str) -> UserAppOperationRecord {
            use shared_types::UserAppLifecycleStore as _;
            let request = shared_types::UserAppAdmission {
                runtime_policy_on_success: None,
                command: None,
                metadata: None,
                app_id: app.into(),
                lifecycle_id: None,
                operation_id: op.into(),
                request_id: Some(format!("req-{op}")),
                request_fingerprint: "a".repeat(64),
                kind: shared_types::UserAppOperationKind::Update,
            };
            match store.admit(&request).await.expect("admission") {
                shared_types::UserAppAdmissionOutcome::Accepted(op) => op,
                shared_types::UserAppAdmissionOutcome::Existing(op) => op,
            }
        }
        let running = admit(&store, "quarantine-running", "op-running").await;
        let running = store
            .advance(&progress(
                &running,
                shared_types::UserAppOperationState::Running,
            ))
            .await
            .expect("claim running");
        let waiting = admit(&store, "quarantine-waiting", "op-waiting").await;
        let waiting = store
            .advance(&progress(
                &waiting,
                shared_types::UserAppOperationState::Running,
            ))
            .await
            .expect("claim waiting");
        let waiting = store
            .advance(&progress(
                &waiting,
                shared_types::UserAppOperationState::WaitingRetry,
            ))
            .await
            .expect("waiting retry");
        let pending = admit(&store, "quarantine-pending", "op-pending").await;
        let succeeded = admit(&store, "quarantine-done", "op-done").await;
        let succeeded = store
            .advance(&progress(
                &succeeded,
                shared_types::UserAppOperationState::Running,
            ))
            .await
            .expect("claim done");
        let succeeded = store
            .advance(&progress(
                &succeeded,
                shared_types::UserAppOperationState::Succeeded,
            ))
            .await
            .expect("done");
        let before_pending = pending.clone();
        let before_succeeded = succeeded.clone();
        store.shutdown().await.expect("shutdown");
        drop(store);

        let reopened = TursoUserAppStore::open_exclusive(&path)
            .await
            .expect("reopen");
        async fn quarantined(
            store: &TursoUserAppStore,
            app: &str,
            op: &str,
            before: UserAppOperationRecord,
        ) {
            use shared_types::UserAppLifecycleStore as _;
            let record = store
                .get_operation(app, op)
                .await
                .expect("read")
                .expect("record");
            assert_eq!(
                record.state,
                shared_types::UserAppOperationState::RecoveryRequired
            );
            assert_eq!(record.revision, before.revision + 1);
            assert_eq!(
                record.error_code.as_deref(),
                Some(shared_types::error_codes::ERR_BACKEND_ERROR)
            );
        }
        quarantined(&reopened, "quarantine-running", "op-running", running).await;
        quarantined(&reopened, "quarantine-waiting", "op-waiting", waiting).await;
        assert_eq!(
            reopened
                .get_operation("quarantine-pending", "op-pending")
                .await
                .expect("read pending")
                .expect("pending record"),
            before_pending,
            "unclaimed pending operations must not be quarantined"
        );
        assert_eq!(
            reopened
                .get_operation("quarantine-done", "op-done")
                .await
                .expect("read done")
                .expect("done record"),
            before_succeeded,
            "terminal operations must not be rewritten"
        );
        reopened.shutdown().await.expect("shutdown");
    }

    /// 反例：隔离扫描选中的在途记录损坏（serde 解析失败）→ 启动整体失败，
    /// 不留部分隔离（其余可隔离记录原样保留）。
    #[tokio::test]
    async fn turso_invalid_interrupted_record_blocks_startup_without_partial_quarantine() {
        use shared_types::UserAppLifecycleStore as _;
        let dir = tempfile::tempdir().expect("dir");
        let path = std::path::absolute(dir.path().join("corrupt.turso.db")).expect("abs");
        let store = TursoUserAppStore::open_exclusive(&path)
            .await
            .expect("open");
        let progress = |op: &UserAppOperationRecord| shared_types::UserAppOperationProgress {
            app_id: op.app_id.clone(),
            operation_id: op.operation_id.clone(),
            lifecycle_id: op.lifecycle_id.clone(),
            expected_revision: op.revision,
            executor_id: "worker-A".into(),
            state: shared_types::UserAppOperationState::Running,
            step: "corrupt-probe".into(),
            checkpoint: serde_json::Value::Null,
            error_code: None,
            error_message: None,
        };
        async fn admit(store: &TursoUserAppStore, app: &str, op: &str) -> UserAppOperationRecord {
            use shared_types::UserAppLifecycleStore as _;
            let request = shared_types::UserAppAdmission {
                runtime_policy_on_success: None,
                command: None,
                metadata: None,
                app_id: app.into(),
                lifecycle_id: None,
                operation_id: op.into(),
                request_id: Some(format!("req-{op}")),
                request_fingerprint: "a".repeat(64),
                kind: shared_types::UserAppOperationKind::Update,
            };
            match store.admit(&request).await.expect("admission") {
                shared_types::UserAppAdmissionOutcome::Accepted(op) => op,
                shared_types::UserAppAdmissionOutcome::Existing(op) => op,
            }
        }
        let victim = admit(&store, "corrupt-victim", "op-corrupt").await;
        let _victim = store
            .advance(&progress(&victim))
            .await
            .expect("claim victim");
        let healthy = admit(&store, "corrupt-healthy", "op-healthy").await;
        let healthy = store
            .advance(&progress(&healthy))
            .await
            .expect("claim healthy");
        let healthy_before = healthy.clone();
        store.shutdown().await.expect("shutdown");
        drop(store);

        // 损坏 victim 记录：json_extract 仍选中（$.state=Running），serde 解析失败
        let db = turso::Builder::new_local(path.to_string_lossy().as_ref())
            .build()
            .await
            .expect("reopen engine");
        let conn = db.connect().expect("connect");
        conn.execute(
            "UPDATE userapp_operations SET record='{\"state\":\"Running\",\"broken\":true}' \
             WHERE operation_id='op-corrupt'",
            (),
        )
        .await
        .expect("corrupt");
        drop(conn);
        drop(db);

        let result = TursoUserAppStore::open_exclusive(&path).await;
        assert!(
            result.is_err(),
            "corrupted interrupted record must block startup"
        );

        // 无部分隔离：healthy 记录原样（直连引擎读——主实例已失败退出）
        let db = turso::Builder::new_local(path.to_string_lossy().as_ref())
            .build()
            .await
            .expect("inspect engine");
        let conn = db.connect().expect("connect");
        let stored: String = conn
            .query(
                "SELECT record FROM userapp_operations WHERE operation_id='op-healthy'",
                (),
            )
            .await
            .expect("select")
            .next()
            .await
            .expect("row")
            .expect("read")
            .get_value(0)
            .expect("column")
            .as_text()
            .expect("text")
            .to_string();
        let parsed: UserAppOperationRecord =
            serde_json::from_str(&stored).expect("healthy record parses");
        assert_eq!(
            parsed, healthy_before,
            "startup failure must not leave a partial quarantine"
        );
    }
}
#[cfg(test)]
mod rows_affected_probe {
    /// T0 探针：Turso execute 返回值语义（INSERT / ON CONFLICT DO NOTHING /
    /// UPDATE / DELETE）——plan §4 要求实测，不能假定与 sqlx 等价。
    #[tokio::test]
    async fn turso_execute_rows_affected_semantics() {
        let dir = tempfile::tempdir().expect("dir");
        let path = std::path::absolute(dir.path().join("probe.turso.db")).expect("abs");
        let db = turso::Builder::new_local(path.to_string_lossy().as_ref())
            .build()
            .await
            .expect("db");
        let conn = db.connect().expect("conn");
        conn.execute_batch(
            "CREATE TABLE t(id TEXT PRIMARY KEY, v INTEGER); INSERT INTO t VALUES('a',1);",
        )
        .await
        .expect("schema");

        // 普通 INSERT：应为 1
        let inserted = conn
            .execute("INSERT INTO t VALUES('b',2)", ())
            .await
            .expect("insert");
        assert_eq!(inserted, 1, "plain INSERT rows_affected");

        // ON CONFLICT DO NOTHING 且无冲突：应为 1（插入生效）
        let inserted = conn
            .execute("INSERT INTO t VALUES('c',3) ON CONFLICT(id) DO NOTHING", ())
            .await
            .expect("insert noc");
        assert_eq!(
            inserted, 1,
            "ON CONFLICT DO NOTHING (no conflict) rows_affected"
        );

        // ON CONFLICT DO NOTHING 且有冲突：应为 0（未生效）
        let skipped = conn
            .execute("INSERT INTO t VALUES('a',9) ON CONFLICT(id) DO NOTHING", ())
            .await
            .expect("insert conflict");
        assert_eq!(
            skipped, 0,
            "ON CONFLICT DO NOTHING (conflict) rows_affected"
        );

        // UPDATE 命中：应为 1
        let updated = conn
            .execute("UPDATE t SET v=10 WHERE id='a'", ())
            .await
            .expect("update");
        assert_eq!(updated, 1, "UPDATE hit rows_affected");

        // UPDATE 零命中：应为 0
        let missed = conn
            .execute("UPDATE t SET v=11 WHERE id='zzz'", ())
            .await
            .expect("update miss");
        assert_eq!(missed, 0, "UPDATE miss rows_affected");

        // DELETE 命中 / 零命中
        let deleted = conn
            .execute("DELETE FROM t WHERE id='b'", ())
            .await
            .expect("del");
        assert_eq!(deleted, 1, "DELETE hit rows_affected");
        let deleted = conn
            .execute("DELETE FROM t WHERE id='zzz'", ())
            .await
            .expect("del miss");
        assert_eq!(deleted, 0, "DELETE miss rows_affected");
    }
}

#[cfg(test)]
mod admit_isolation_probe {
    use super::*;

    /// 隔离 admit 的 VersionConflict 来源：手动执行完整 SQL 序列
    /// （fresh app / Stop / 无重复请求）逐段检查 rows_affected。
    #[tokio::test]
    async fn turso_admit_manual_sql_sequence() {
        let dir = tempfile::tempdir().expect("dir");
        let path = std::path::absolute(dir.path().join("iso.turso.db")).expect("abs");
        let (store, handle) = TursoUserAppStore::spawn_worker(&path).await.expect("spawn");
        let detail: Result<String, Error> = store
            .run(|conn: &mut turso::Connection| {
                Box::pin(async move {
                    use super::exec::{begin, text};
                    let mut tx = begin(conn).await?;
                    // ① INSERT lifecycle ON CONFLICT DO NOTHING（fresh）
                    let inserted = tx
                        .exec(
                            "INSERT INTO userapp_lifecycles(app_id,record) VALUES(?1,?2) ON CONFLICT(app_id) DO NOTHING",
                            vec![text("iso-app"), text("{}")],
                        )
                        .await?;
                    assert_eq!(inserted, 1, "lifecycle insert");
                    // ② 读回 locked
                    let encoded = tx
                        .opt_string(
                            "SELECT record FROM userapp_lifecycles WHERE app_id=?1",
                            vec![text("iso-app")],
                        )
                        .await?;
                    assert!(encoded.is_some(), "locked readback");
                    // ③ INSERT operation（terminal=0）
                    let inserted = tx
                        .exec(
                            "INSERT INTO userapp_operations(operation_id,app_id,request_id,terminal,record) VALUES(?1,?2,?3,0,?4)",
                            vec![text("iso-op"), text("iso-app"), text("iso-req"), text("{}")],
                        )
                        .await?;
                    assert_eq!(inserted, 1, "operation insert");
                    // ④ UPDATE lifecycle SET record（admit 的 CAS 段）
                    let updated = tx
                        .exec(
                            "UPDATE userapp_lifecycles SET record=?2 WHERE app_id=?1",
                            vec![text("iso-app"), text(r#"{"rev":1}"#)],
                        )
                        .await?;
                    assert_eq!(updated, 1, "lifecycle update inside tx, got {updated}");
                    // ⑤ INSERT alias ON CONFLICT DO NOTHING
                    let aliased = tx
                        .exec(
                            "INSERT INTO userapp_operation_requests(app_id,request_id,operation_id) VALUES(?1,?2,?3) ON CONFLICT(app_id,request_id) DO NOTHING",
                            vec![text("iso-app"), text("iso-req"), text("iso-op")],
                        )
                        .await?;
                    assert_eq!(aliased, 1, "alias insert");
                    tx.commit().await?;
                    Ok(TaskOut(Box::new("manual sequence all ok".to_string())))
                })
            })
            .await;
        handle.shutdown.send(true).ok();
        let message = detail.expect("manual admit SQL sequence");
        assert_eq!(&*message, "manual sequence all ok");
    }
}
