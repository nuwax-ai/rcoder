//! Turso 本地控制存储（specs/userapp-turso-local-storage plan §3）。
//!
//! 架构：专用数据库线程 + 有界队列。线程内运行 current-thread tokio
//! runtime 并独占 Connection；队列单位是**完整 store 方法**（Boxed future
//! 工厂），线程内顺序完成全部语句 + commit/rollback 再处理下一任务——
//! 不同请求的事务不会在同一连接上交错。调用方丢弃 future 不中止已开始
//! 的事务（oneshot 发送失败即丢弃回包，事务照常完成——可能已提交但
//! 回包丢失，调用方按原 request_id/operation_id 幂等查询）。
//!
//! 生命周期不变量（R01/R03）：目录锁在线程启动前获取、随线程闭包移动，
//! 仅在线程退出（连接 Drop 之后）才释放——任何错误/取消/Drop 路径都
//! 不能让锁先于连接销毁。shutdown 的完成结果由所有调用方共享。

mod exec;
mod migrations;
mod ops;
mod restart;

use std::sync::Arc;

use futures::future::BoxFuture;
use shared_types::{UserAppLifecycleRecord, UserAppOperationRecord, UserAppStoreError as Error};
use tokio::sync::{mpsc, oneshot};

use crate::userapp_lifecycle::storage;

/// 队列深度（有界——满时有界等待后明确拒绝，不静默丢写）。
const QUEUE_DEPTH: usize = 256;

/// 队列满时的容量等待预算（plan §3：入队等待必须有界；超时即拒绝，
/// 拒绝语义保证任务**从未入队**＝未执行）。
const QUEUE_WAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);
const QUEUE_WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// 队列满/关机时的明确错误（plan §3 取消与 deadline）。
fn unavailable(reason: &str) -> Error {
    Error::Storage(anyhow::anyhow!(
        "userapp store worker unavailable: {reason}"
    ))
}

/// 旧 SQLite 库同目录残留检测（R05）：默认旧库文件名存在即视为旧库。
fn legacy_sqlite_sibling(db_path: &std::path::Path) -> bool {
    db_path
        .parent()
        .is_some_and(|dir| dir.join("userapp.sqlite3").exists())
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
    /// 关机协调句柄（装配层持有；Drop 只发信号作兜底，不 join）。
    worker: std::sync::Mutex<Option<Arc<WorkerHandle>>>,
}

/// 关机协调：所有调用共享同一完成结果（trait-design §6）。
///
/// 第一个调用者发送停止信号并在独立 spawn 的 blocking 任务里 join；
/// join 结果写入 `outcome` 并经 `finished` 唤醒全部等待者。等待者
/// future 被取消不影响关闭动作（join 不依赖任何调用方存活）。
struct WorkerHandle {
    shutdown: tokio::sync::watch::Sender<bool>,
    /// 第一次 shutdown 时取走 join 句柄并启动收束任务。
    join: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    outcome: std::sync::Mutex<Option<Result<(), Error>>>,
    finished: tokio::sync::Notify,
}

impl TursoUserAppStore {
    /// 独占打开：目录锁 → 旧库目录保护 → worker/连接 → PRAGMA 校验 →
    /// 迁移 → 重启隔离。锁移动进 worker 线程，覆盖连接全部生命周期
    /// （plan §3 启动顺序）；启动失败路径显式关停并 join 后才返回 Err。
    pub async fn open_exclusive(path: &std::path::Path) -> Result<Self, Error> {
        let path = path.to_path_buf();
        // 独占目录校验复用后端中性实现（别名/远程 fs 保护）
        let (db_path, lock) =
            tokio::task::spawn_blocking(move || super::exclusive_directory::acquire(&path))
                .await
                .map_err(storage)??;
        // R05：旧库目录保护（spec 行为要求 7）——新库不存在而目录已有
        // 旧 userapp.sqlite3 时拒绝启动，指引用独立目录；不删除、不迁移。
        if !db_path.exists() && legacy_sqlite_sibling(&db_path) {
            return Err(Error::InvalidOperation(
                "data directory contains a legacy userapp.sqlite3 without userapp.turso.db; \
                 refusing to silently initialize a second application identity — \
                 use a fresh data directory (spec 行为要求 7)"
                    .into(),
            ));
        }
        let (store, handle) = Self::spawn_worker(&db_path, lock).await?;
        *store.worker.lock().map_err(|_| unavailable("poisoned"))? = Some(Arc::new(handle));
        // 重启隔离（在 worker 侧、对外 ready 前执行）。失败路径必须先
        // 关停 worker（排空 + join + 释放锁）再返回错误。
        let isolated: Result<(), Error> = store
            .run(|conn: &mut turso::Connection| {
                Box::pin(async move {
                    restart::quarantine(conn)
                        .await
                        .map(|r| TaskOut(Box::new(r)))
                })
            })
            .await;
        if let Err(error) = isolated {
            drop(store.shutdown().await);
            return Err(error);
        }
        Ok(store)
    }

    /// 内部：起 worker 线程并就绪（连接+PRAGMA+迁移已完成）。
    ///
    /// `lock`（目录独占锁）随闭包移动进线程：线程退出前 Drop 连接、
    /// 最后 Drop 锁——锁始终覆盖连接及在途事务生命周期。
    async fn spawn_worker(
        db_path: &std::path::Path,
        lock: std::fs::File,
    ) -> Result<(Self, WorkerHandle), Error> {
        let (ready_tx, ready_rx) = oneshot::channel::<
            Result<(mpsc::Sender<Task>, tokio::sync::watch::Sender<bool>), Error>,
        >();
        let db_path = db_path.to_path_buf();
        let join = std::thread::Builder::new()
            .name("userapp-turso-store".into())
            .spawn(move || {
                // 锁在本线程持有至退出（连接 Drop 之后才释放）
                let _directory_lock = lock;
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        // ready 接收方消失（调用方取消）同样走此路径：发送
                        // 失败即丢弃，线程返回，锁随闭包释放。
                        drop(ready_tx.send(Err(storage(anyhow::anyhow!(
                            "build turso store runtime: {error}"
                        )))));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let (tx, mut rx) = mpsc::channel::<Task>(QUEUE_DEPTH);
                    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
                    // 连接 + PRAGMA + 迁移（worker 内完成——连接不跨线程）
                    let mut conn = match init_connection(&db_path).await {
                        Ok(conn) => {
                            if ready_tx.send(Ok((tx.clone(), shutdown_tx))).is_err() {
                                // 调用方取消：立即终止，不进入主循环
                                return;
                            }
                            conn
                        }
                        Err(error) => {
                            drop(ready_tx.send(Err(error)));
                            return;
                        }
                    };
                    // 连接隔离标志：错误路径的显式回滚失败后置位，后续
                    // 任务全部快速拒绝（R04：回滚失败 ⇒ 后端不可写）。
                    let mut isolated = false;
                    // 主循环：顺序处理完整方法。终止条件：shutdown 信号、
                    // shutdown sender 全部消失（Err）、任务通道关闭。
                    // 终止前先排空已接收任务再退出。
                    let mut stopping = false;
                    while !stopping {
                        tokio::select! {
                            biased;
                            changed = shutdown_rx.changed() => {
                                match changed {
                                    Err(_) => stopping = true, // sender 消失：终止
                                    Ok(()) => {
                                        if *shutdown_rx.borrow() {
                                            stopping = true;
                                        }
                                    }
                                }
                            }
                            task = rx.recv() => {
                                match task {
                                    Some(task) => execute_task(&mut conn, &mut isolated, task).await,
                                    None => stopping = true,
                                }
                            }
                        }
                    }
                    rx.close();
                    while let Some(task) = rx.recv().await {
                        execute_task(&mut conn, &mut isolated, task).await;
                    }
                    // 连接先于锁销毁（lock 是闭包局部变量，最后 Drop）
                });
            })
            .map_err(|e| storage(anyhow::anyhow!("spawn turso store worker: {e}")))?;
        match ready_rx.await {
            Ok(Ok((tx, shutdown))) => Ok((
                Self {
                    tx,
                    worker: std::sync::Mutex::new(None),
                },
                WorkerHandle {
                    shutdown,
                    join: std::sync::Mutex::new(Some(join)),
                    outcome: std::sync::Mutex::new(None),
                    finished: tokio::sync::Notify::new(),
                },
            )),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(unavailable("worker died during init")),
        }
    }

    /// 在 worker 上执行一个完整方法体。
    ///
    /// 入队使用 try_send + 有界容量等待：任何返回错误的路径都保证任务
    /// **从未入队**（＝未执行），不存在"已入队却报未执行"的竞态；
    /// 入队后调用方丢弃 future 不中止事务（oneshot drop → 结果丢弃）。
    async fn run<R: Send + 'static>(
        &self,
        body: impl for<'c> FnOnce(&'c mut turso::Connection) -> BoxFuture<'c, Result<TaskOut, Error>>
        + Send
        + 'static,
    ) -> Result<R, Error> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let mut pending = Task {
            body: Box::new(body),
            reply: reply_tx,
        };
        let deadline = tokio::time::Instant::now() + QUEUE_WAIT_BUDGET;
        loop {
            match self.tx.try_send(pending) {
                Ok(()) => break,
                Err(mpsc::error::TrySendError::Full(task)) => {
                    pending = task;
                    if tokio::time::Instant::now() >= deadline {
                        return Err(unavailable(&format!(
                            "queue full after {}s (depth {QUEUE_DEPTH}); task never enqueued",
                            QUEUE_WAIT_BUDGET.as_secs()
                        )));
                    }
                    tokio::time::sleep(QUEUE_WAIT_POLL).await;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(unavailable("worker stopped"));
                }
            }
        }
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

    /// 显式关机（装配层）：停生产者 → 发送停止信号 → 排空队列 → 关连接
    /// → join → 释放目录锁。所有调用方等待同一完成结果；join panic 与
    /// 线程错误显式传播；关闭动作独立于任何调用方 future 存活。
    pub async fn shutdown(&self) -> Result<(), Error> {
        let handle = {
            self.worker
                .lock()
                .map_err(|_| unavailable("poisoned"))?
                .clone()
        };
        let Some(handle) = handle else {
            return Ok(()); // 从未安装（启动失败路径已自行 join）
        };
        // 启动收束（仅一次）：发送信号 + detached join，结果写入共享槽
        {
            let mut guard = handle.join.lock().map_err(|_| unavailable("poisoned"))?;
            if let Some(join) = guard.take() {
                handle
                    .shutdown
                    .send(true)
                    .map_err(|_| unavailable("shutdown channel closed"))?;
                let waiter = Arc::clone(&handle);
                // detached：不依赖任何 shutdown 调用方存活
                tokio::task::spawn_blocking(move || {
                    let outcome = match join.join() {
                        Ok(()) => Ok(()),
                        Err(panic) => Err(storage(anyhow::anyhow!(
                            "userapp store worker thread failed during shutdown: {panic:?}"
                        ))),
                    };
                    if let Ok(mut slot) = waiter.outcome.lock() {
                        *slot = Some(outcome);
                    }
                    waiter.finished.notify_waiters();
                });
            }
        }
        // 等待共享完成结果
        loop {
            {
                let guard = handle.outcome.lock().map_err(|_| unavailable("poisoned"))?;
                if let Some(outcome) = guard.as_ref() {
                    return match outcome {
                        Ok(()) => Ok(()),
                        Err(error) => Err(storage(anyhow::anyhow!("{error:#}"))),
                    };
                }
            }
            handle.finished.notified().await;
        }
    }
}

impl Drop for TursoUserAppStore {
    fn drop(&mut self) {
        // Drop 只作兜底（不构成 flush 完成证据）：发送停止信号，不 join
        // （同步 Drop 不能阻塞等待线程）。worker 持有目录锁，即便本
        // 结构体先消失，第二实例在 worker 退出（连接销毁、锁释放）
        // 之前仍会被独占锁拒绝——不变量不被破坏。
        if let Ok(mut guard) = self.worker.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.shutdown.send(true);
        }
    }
}

async fn execute_task(conn: &mut turso::Connection, isolated: &mut bool, task: Task) {
    if *isolated {
        // R04：回滚清理失败后连接被隔离——后续任务快速失败，不静默复用
        drop(task.reply.send(Err(unavailable(
            "connection isolated after rollback failure; store requires restart",
        ))));
        return;
    }
    let body = task.body;
    let reply = task.reply;
    let result = body(conn).await;
    if result.is_err() {
        // R04：错误路径的显式清理——立即以一条轻量语句触发
        // maybe_handle_dangling_tx（连接上任何语句前先执行挂起的
        // ROLLBACK），不把清理推迟到下一个业务请求；清理失败则隔离
        // 连接（后续任务全部拒绝），原错误照常返回给原调用方。
        if let Err(cleanup_error) = conn.query("SELECT 1", ()).await {
            *isolated = true;
            tracing::error!(
                error = %cleanup_error,
                "userapp store rollback cleanup failed; connection isolated"
            );
        }
    }
    // 回包丢失（调用方取消）→ 结果丢弃
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

#[cfg(test)]
mod tests;

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

/// 关机控制（trait-design §6）：与业务门面同一 Arc，装配层单独持有
/// `Arc<dyn UserAppStoreControl>`；停生产者 → 排空队列 → 关连接 → join
/// 线程 → 释放目录锁（锁在 worker 线程内，随线程退出释放）。
#[async_trait::async_trait]
impl crate::userapp_lifecycle::control::UserAppStoreControl for TursoUserAppStore {
    async fn shutdown(&self) -> Result<(), Error> {
        TursoUserAppStore::shutdown(self).await
    }
}
