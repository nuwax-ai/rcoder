//! Turso 后端测试（从 mod.rs 拆出——2026-09-19 文件拆分）。
//! 组件契约套件在 userapp_lifecycle/tests.rs（后端中性）；此处是
//! worker/生命周期/关机/队列/目录保护等 Turso 专属结构测试与 T0 探针。

use std::sync::Arc;

use super::{Error, QUEUE_DEPTH, QUEUE_WAIT_BUDGET, TaskOut, TursoUserAppStore, exec, storage};
use shared_types::UserAppOperationRecord;

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

// ===== R01/R03/R04/R05 回归（源自 2026-09-19 复核探针，修复前全部失败）=====

/// R05：旧库目录保护——目录只有旧 userapp.sqlite3 时拒绝启动且不产生新库。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turso_legacy_sqlite_directory_is_rejected() {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(
        dir.path().join("userapp.sqlite3"),
        b"SQLite format 3\x00dummy",
    )
    .expect("old db");
    let path = std::path::absolute(dir.path().join("userapp.turso.db")).expect("abs");
    let result = TursoUserAppStore::open_exclusive(&path).await;
    let Err(error) = result else {
        panic!("旧库存在且新库不存在时应 fail-fast 拒绝启动")
    };
    assert!(
        matches!(error, Error::InvalidOperation(ref m) if m.contains("userapp.sqlite3")),
        "应给出独立目录指引：{error}"
    );
    assert!(!path.exists(), "拒绝路径不得创建新库文件");
    let legacy = dir.path().join("userapp.sqlite3");
    assert!(legacy.exists(), "旧库文件不得被删除或改动");
    // 空目录正常打开（对照）
    let fresh = tempfile::tempdir().expect("fresh");
    let fresh_path = std::path::absolute(fresh.path().join("userapp.turso.db")).expect("abs");
    let store = TursoUserAppStore::open_exclusive(&fresh_path)
        .await
        .expect("空目录可正常初始化");
    store.shutdown().await.expect("shutdown");
}

/// R05 既有新库：旧库并存时按新配置正常打开（不拒绝）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turso_existing_new_db_with_legacy_sibling_opens() {
    let dir = tempfile::tempdir().expect("dir");
    let path = std::path::absolute(dir.path().join("userapp.turso.db")).expect("abs");
    {
        let store = TursoUserAppStore::open_exclusive(&path)
            .await
            .expect("first open");
        store.ensure_identity("existing").await.expect("identity");
        store.shutdown().await.expect("shutdown");
    }
    std::fs::write(dir.path().join("userapp.sqlite3"), b"legacy-later").expect("legacy");
    let reopened = TursoUserAppStore::open_exclusive(&path)
        .await
        .expect("reopen with legacy");
    assert!(
        reopened
            .get_application("existing")
            .await
            .expect("get")
            .is_some()
    );
    reopened.shutdown().await.expect("shutdown");
}

/// R01：quarantine 失败（损坏在途记录）后 worker 必须退出并释放连接——
/// 进程不得再持有该数据库文件 fd（原缺陷：忙循环 + 线程泄漏）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turso_quarantine_failure_releases_worker_and_lock() {
    let dir = tempfile::tempdir().expect("dir");
    let path = std::path::absolute(dir.path().join("leak.turso.db")).expect("abs");
    let request = shared_types::UserAppAdmission {
        runtime_policy_on_success: None,
        command: None,
        metadata: None,
        app_id: "leak-app".into(),
        lifecycle_id: None,
        operation_id: "leak-op".into(),
        request_id: Some("leak-req".into()),
        request_fingerprint: "a".repeat(64),
        kind: shared_types::UserAppOperationKind::Update,
    };
    {
        let store = TursoUserAppStore::open_exclusive(&path)
            .await
            .expect("open");
        let op = match store.admit(&request).await.expect("admit") {
            shared_types::UserAppAdmissionOutcome::Accepted(op) => op,
            shared_types::UserAppAdmissionOutcome::Existing(op) => op,
        };
        let progress = shared_types::UserAppOperationProgress {
            app_id: op.app_id.clone(),
            operation_id: op.operation_id.clone(),
            lifecycle_id: op.lifecycle_id.clone(),
            expected_revision: op.revision,
            executor_id: "w".into(),
            state: shared_types::UserAppOperationState::Running,
            step: "s".into(),
            checkpoint: serde_json::Value::Null,
            error_code: None,
            error_message: None,
        };
        store.advance(&progress).await.expect("running");
        store.shutdown().await.expect("shutdown");
    }
    let db = turso::Builder::new_local(path.to_string_lossy().as_ref())
        .build()
        .await
        .expect("engine");
    let conn = db.connect().expect("conn");
    conn.execute(
        "UPDATE userapp_operations SET record='{\"state\":\"Running\",\"broken\":true}' \
         WHERE operation_id='leak-op'",
        (),
    )
    .await
    .expect("corrupt");
    drop(conn);
    drop(db);
    // 打开失败（quarantine 解析失败）；失败路径应已关停并 join worker
    let result = TursoUserAppStore::open_exclusive(&path).await;
    assert!(result.is_err(), "损坏记录应阻断启动");
    // 进程不得再持有 db/WAL fd（worker 已退出）
    std::thread::sleep(std::time::Duration::from_millis(200));
    let pid = std::process::id();
    let lsof = std::process::Command::new("lsof")
        .args(["-p", &pid.to_string(), "-F", "n"])
        .output()
        .expect("lsof");
    let out = String::from_utf8_lossy(&lsof.stdout);
    let db_name = path.file_name().unwrap().to_string_lossy().to_string();
    let holding: Vec<&str> = out
        .lines()
        .filter(|l| l.starts_with('n') && l.contains(&db_name))
        .collect();
    assert!(
        holding.is_empty(),
        "quarantine 失败后 worker 应退出并释放连接；仍持有 fd：{holding:?}"
    );
    // worker 退出 ⇒ 锁已释放 ⇒ 可再次尝试打开（仍因损坏失败，但不因锁）
    let again = TursoUserAppStore::open_exclusive(&path).await;
    assert!(again.is_err(), "损坏记录持续阻断（非锁原因）");
}

/// R03：并发 shutdown 的第二个调用必须等待同一完成结果，不得在
/// worker 仍在排空在途任务时提前返回。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turso_concurrent_shutdown_waits_for_same_completion() {
    let dir = tempfile::tempdir().expect("dir");
    let path = std::path::absolute(dir.path().join("shutdown.turso.db")).expect("abs");
    let store = Arc::new(
        TursoUserAppStore::open_exclusive(&path)
            .await
            .expect("open"),
    );
    let slow = Arc::clone(&store);
    tokio::spawn(async move {
        drop(
            slow.run::<String>(|_conn: &mut turso::Connection| {
                Box::pin(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                    Ok(TaskOut(Box::new("slow-done".to_string())))
                })
            })
            .await,
        );
    });
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let first = {
        let s = Arc::clone(&store);
        tokio::spawn(async move { s.shutdown().await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let started = std::time::Instant::now();
    let second = Arc::clone(&store).shutdown().await;
    let second_elapsed = started.elapsed();
    assert!(second.is_ok(), "第二个 shutdown 应成功：{second:?}");
    first.await.expect("join first").expect("first shutdown");
    assert!(
        second_elapsed >= std::time::Duration::from_millis(300),
        "第二个 shutdown 在 {second_elapsed:?} 内提前返回——未等待 worker 实际关闭完成"
    );
}

/// R03：重复 shutdown 与 worker panic 传播。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turso_shutdown_repeats_share_result_and_panic_propagates() {
    let dir = tempfile::tempdir().expect("dir");
    let path = std::path::absolute(dir.path().join("panic.turso.db")).expect("abs");
    let store = TursoUserAppStore::open_exclusive(&path)
        .await
        .expect("open");
    store.shutdown().await.expect("first shutdown");
    // 重复调用共享同一成功结果
    store
        .shutdown()
        .await
        .expect("repeat shutdown shares result");
    // panic 路径：注入 worker panic，shutdown 必须报错而非 Ok
    let dir2 = tempfile::tempdir().expect("dir2");
    let path2 = std::path::absolute(dir2.path().join("panic2.turso.db")).expect("abs");
    let store2 = TursoUserAppStore::open_exclusive(&path2)
        .await
        .expect("open2");
    let result: Result<String, Error> = store2
        .run(|_conn: &mut turso::Connection| {
            Box::pin(async move { panic!("regression: injected worker panic") })
        })
        .await;
    assert!(result.is_err(), "panic 后 run 应报 worker 不可用");
    let shutdown = store2.shutdown().await;
    assert!(
        shutdown.is_err(),
        "worker 线程 panic 后 shutdown 不得返回 Ok（join 结果被丢弃）"
    );
}

/// R04：队列满时第 257 个请求在有界时间内得到明确拒绝（从未入队）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turso_queue_full_fails_within_budget() {
    let dir = tempfile::tempdir().expect("dir");
    let path = std::path::absolute(dir.path().join("queue.turso.db")).expect("abs");
    let store = Arc::new(
        TursoUserAppStore::open_exclusive(&path)
            .await
            .expect("open"),
    );
    let mut tasks = Vec::new();
    for _ in 0..QUEUE_DEPTH {
        let s = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            drop(
                s.run::<String>(|_conn: &mut turso::Connection| {
                    Box::pin(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                        Ok(TaskOut(Box::new("q".to_string())))
                    })
                })
                .await,
            );
        }));
    }
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    // 预算调短不可行（常量）；用真实预算验证：10s 内必然得到明确错误
    // 或成功（队列被消费）——两者都不是"无限等待"。为测试时效，
    // 断言在 2×预算内必有结果。
    let started = std::time::Instant::now();
    let overflow = tokio::time::timeout(
        QUEUE_WAIT_BUDGET.saturating_mul(2) + std::time::Duration::from_secs(1),
        {
            let s = Arc::clone(&store);
            async move {
                drop(
                    s.run::<String>(|_conn: &mut turso::Connection| {
                        Box::pin(async move { Ok(TaskOut(Box::new("overflow".to_string()))) })
                    })
                    .await,
                );
            }
        },
    )
    .await;
    let elapsed = started.elapsed();
    assert!(
        overflow.is_ok(),
        "队列满时溢出请求应在有界时间内得到结果；实际 {elapsed:?} 内无结果（无限等待）"
    );
    for t in tasks {
        drop(t.await);
    }
    store.shutdown().await.expect("cleanup shutdown");
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
        message.contains("checksum mismatch") || message.contains("modified after being applied"),
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
    use super::{Error, TaskOut, TursoUserAppStore};

    /// 隔离 admit 的 VersionConflict 来源：手动执行完整 SQL 序列
    /// （fresh app / Stop / 无重复请求）逐段检查 rows_affected。
    #[tokio::test]
    async fn turso_admit_manual_sql_sequence() {
        let dir = tempfile::tempdir().expect("dir");
        let path = std::path::absolute(dir.path().join("iso.turso.db")).expect("abs");
        let path_c = path.clone();
        let (db_path, lock) = tokio::task::spawn_blocking(move || {
            crate::userapp_lifecycle::exclusive_directory::acquire(&path_c).expect("acquire")
        })
        .await
        .expect("lock");
        let (store, handle) = TursoUserAppStore::spawn_worker(&db_path, lock)
            .await
            .expect("spawn");
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
