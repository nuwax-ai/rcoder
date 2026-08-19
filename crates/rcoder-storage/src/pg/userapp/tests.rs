//! UserApp PG 集成测试(publish_tasks / userapp_metadata)。
//!
//! 运行条件与主测试套一致:`RCODER_PG_TEST_DSN` 指向可破坏的测试库,
//! 未设置时静默跳过。公共 helper(test_dsn/wait_for/uuid_suffix)复用
//! `crate::pg::test_support`。

#![cfg(all(test, feature = "pg"))]

use sqlx::postgres::PgPoolOptions;

use crate::pg::test_support::{DSN_ENV, test_dsn, uuid_suffix};

/// publish_tasks 域测试互斥锁:roundtrip 的 recover_running/purge_expired 是
/// 全库 UPDATE/DELETE,并行会打掉其他测试的活任务行。域内测试串行(跨域无影响)。
/// tokio Mutex(非 std):guard 需跨测试内 await 持有。
static PUBLISH_DOMAIN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn publish_persistence_roundtrip_and_constraints() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let _domain_guard = PUBLISH_DOMAIN_LOCK.lock().await;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&dsn)
        .await
        .expect("test pool");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrate");
    let repo = crate::pg::userapp::publish::PgPublishTaskPersistence::new(pool.clone());
    use crate::publish_repo::PublishTaskPersistence as _;
    let app_id = format!("pubapp-{}", uuid_suffix());
    let (task_a, task_b) = (format!("task-a-{app_id}"), format!("task-b-{app_id}"));

    let record = |task_id: &str, state: &str| crate::publish_repo::PublishTaskRecord {
        task_id: task_id.into(),
        app_id: app_id.clone(),
        project_id: app_id.clone(),
        kind: "build".into(),
        state: state.into(),
        stage: None,
        release_id: None,
        error: None,
        progress: None,
        owner_pod: Some("test-pod".into()),
        created_at: chrono::Utc::now(),
        terminal_at: None,
    };

    // create + 唯一约束（同 app 活跃任务互斥）
    repo.create(&record(&task_a, "running"))
        .await
        .expect("create");
    let busy = repo.create(&record(&task_b, "running")).await;
    assert!(
        matches!(busy, Err(crate::publish_repo::PublishRepoError::Busy(_))),
        "second active task for same app must conflict"
    );

    // update_terminal 释放活跃槽位后可再建
    repo.update_terminal(
        &task_a,
        "completed",
        chrono::Utc::now(),
        None,
        Some("rel-1"),
    )
    .await
    .expect("terminal");
    repo.create(&record(&task_b, "running"))
        .await
        .expect("create after terminal");

    // get 回读
    let got = repo.get(&task_a).await.expect("get").expect("row");
    assert_eq!(got.state, "completed");
    assert_eq!(got.release_id.as_deref(), Some("rel-1"));

    // recover_running：task-b（owner=test-pod，本副本）标记 failed
    let recovered = repo
        .recover_running(
            "rcoder restarted",
            "test-pod",
            chrono::Utc::now() - chrono::Duration::hours(3),
        )
        .await
        .expect("recover");
    assert!(recovered >= 1);
    let got = repo.get(&task_b).await.expect("get").expect("row");
    assert_eq!(got.state, "failed");

    // purge_expired：终态行过期清理（created_at 很久以前不影响——按 terminal_at；此处造一条过期行）
    repo.update_terminal(
        &task_a,
        "failed",
        chrono::Utc::now() - chrono::Duration::hours(25),
        Some("test"),
        None,
    )
    .await
    .expect("terminal old");
    let purged = repo.purge_expired(24 * 3600).await.expect("purge");
    assert!(purged >= 1, "expired terminal row must be purged");
    assert!(repo.get(&task_a).await.expect("get").is_none());
}

/// recover_running 的多副本安全：只收敛"本副本的 + 无主的 + 超时僵尸"，
/// 其他副本的活跃任务不动（滚动更新新旧 Pod 并存窗口不误杀）。
#[tokio::test]
async fn recover_running_respects_owner_pod_and_staleness() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let _domain_guard = PUBLISH_DOMAIN_LOCK.lock().await;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&dsn)
        .await
        .expect("test pool");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations");
    let repo = crate::pg::userapp::publish::PgPublishTaskPersistence::new(pool.clone());
    use crate::publish_repo::PublishTaskPersistence as _;

    let suffix = uuid_suffix();
    let record = |task_id: &str, app_id: &str, owner: Option<&str>, age: chrono::Duration| {
        crate::publish_repo::PublishTaskRecord {
            task_id: task_id.into(),
            app_id: app_id.into(),
            project_id: app_id.into(),
            kind: "build".into(),
            state: "running".into(),
            stage: None,
            release_id: None,
            error: None,
            progress: None,
            owner_pod: owner.map(str::to_string),
            created_at: chrono::Utc::now() - age,
            terminal_at: None,
        }
    };

    // 四个 app 各一条活行：本副本 / 其他副本 / 无主 / 其他副本的超时僵尸
    // （后者兼模拟"Pod 重建改名后 owner 失配，靠 stale 兜底"的场景）
    let mine = format!("task-mine-{suffix}");
    let other = format!("task-other-{suffix}");
    let noowner = format!("task-noown-{suffix}");
    let stale = format!("task-stale-{suffix}");
    repo.create(&record(
        &mine,
        &format!("app-mine-{suffix}"),
        Some("pod-self"),
        chrono::Duration::zero(),
    ))
    .await
    .expect("create mine");
    repo.create(&record(
        &other,
        &format!("app-other-{suffix}"),
        Some("pod-other"),
        chrono::Duration::zero(),
    ))
    .await
    .expect("create other");
    repo.create(&record(
        &noowner,
        &format!("app-noown-{suffix}"),
        None,
        chrono::Duration::zero(),
    ))
    .await
    .expect("create noowner");
    repo.create(&record(
        &stale,
        &format!("app-stale-{suffix}"),
        Some("pod-other"),
        chrono::Duration::hours(3),
    ))
    .await
    .expect("create stale");

    let recovered = repo
        .recover_running(
            "test recovery",
            "pod-self",
            chrono::Utc::now() - chrono::Duration::hours(2),
        )
        .await
        .expect("recover");
    // mine（owner 命中）+ noowner（无主）+ stale（超时僵尸）；other 不动
    assert_eq!(recovered, 3, "own + unowned + stale rows converged");

    for id in [&mine, &noowner, &stale] {
        let got = repo.get(id).await.expect("get").expect("row");
        assert_eq!(got.state, "failed", "{id} must be converged");
    }
    let got = repo.get(&other).await.expect("get").expect("row");
    assert_eq!(
        got.state, "running",
        "another replica's active task must NOT be killed by this pod's recovery"
    );

    // 清理四条行防污染共享库（终态行由 purge_expired 兜底回收）
    for id in [&mine, &other, &noowner, &stale] {
        if let Err(cleanup_err) = repo
            .update_terminal(id, "failed", chrono::Utc::now(), Some("test cleanup"), None)
            .await
        {
            eprintln!("[cleanup] {id}: {cleanup_err}");
        }
    }
}

/// list:app_ids/kind/active_only 过滤、created_at DESC 排序、分页与 total。
/// 过滤一律带 app_ids 限定,防同库其他测试行污染断言。
#[tokio::test]
async fn publish_list_filters_and_pagination() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let _domain_guard = PUBLISH_DOMAIN_LOCK.lock().await;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&dsn)
        .await
        .expect("test pool");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrate");
    let repo = crate::pg::userapp::publish::PgPublishTaskPersistence::new(pool);
    use crate::publish_repo::PublishTaskPersistence as _;
    let suffix = uuid_suffix();
    let (app1, app2) = (format!("listapp1-{suffix}"), format!("listapp2-{suffix}"));
    let now = chrono::Utc::now();

    let record = |task_id: &str, app_id: &str, kind: &str, created_at| {
        crate::publish_repo::PublishTaskRecord {
            task_id: task_id.into(),
            app_id: app_id.to_string(),
            project_id: app_id.to_string(),
            kind: kind.into(),
            state: "running".into(),
            stage: None,
            release_id: None,
            error: None,
            progress: None,
            owner_pod: None,
            created_at,
            terminal_at: None,
        }
    };
    // t1 终态(app1/build,最旧)、t2 活(app1/publish)、t3 活(app2/build,最新)。
    let t1 = format!("t1-{suffix}");
    let t2 = format!("t2-{suffix}");
    let t3 = format!("t3-{suffix}");
    repo.create(&record(
        &t1,
        &app1,
        "build",
        now - chrono::Duration::hours(3),
    ))
    .await
    .expect("create t1");
    // U2 同 app 单活跃任务:t2(app1)须等 t1 终态后才可创建。
    repo.update_terminal(
        &t1,
        "completed",
        now - chrono::Duration::hours(2),
        None,
        None,
    )
    .await
    .expect("terminal t1");
    repo.create(&record(
        &t2,
        &app1,
        "publish",
        now - chrono::Duration::hours(2),
    ))
    .await
    .expect("create t2");
    repo.create(&record(
        &t3,
        &app2,
        "build",
        now - chrono::Duration::hours(1),
    ))
    .await
    .expect("create t3");

    let scoped = crate::publish_repo::PublishTaskQuery {
        app_ids: Some(vec![app1.clone(), app2.clone()]),
        kind: None,
        active_only: false,
    };

    // 全量:created_at DESC → t3, t2, t1。
    let result = repo.list(&scoped, 1, 10).await.expect("list all");
    assert_eq!(result.total, 3);
    let ids: Vec<&str> = result.items.iter().map(|r| r.task_id.as_str()).collect();
    assert_eq!(ids, vec![t3.as_str(), t2.as_str(), t1.as_str()]);

    // active_only:只剩 t2, t3。
    let result = repo
        .list(
            &crate::publish_repo::PublishTaskQuery {
                active_only: true,
                ..scoped.clone()
            },
            1,
            10,
        )
        .await
        .expect("list active");
    assert_eq!(result.total, 2);
    assert!(result.items.iter().all(|r| r.terminal_at.is_none()));

    // kind=publish:仅 t2。
    let result = repo
        .list(
            &crate::publish_repo::PublishTaskQuery {
                kind: Some("publish".into()),
                ..scoped.clone()
            },
            1,
            10,
        )
        .await
        .expect("list by kind");
    assert_eq!(result.total, 1);
    assert_eq!(result.items[0].task_id, t2);

    // app_ids=[app1]:t2, t1。
    let result = repo
        .list(
            &crate::publish_repo::PublishTaskQuery {
                app_ids: Some(vec![app1.clone()]),
                ..scoped.clone()
            },
            1,
            10,
        )
        .await
        .expect("list by app");
    assert_eq!(result.total, 2);

    // 分页:page=1 size=2 → [t3, t2];page=2 → [t1];total 恒 3。
    let result = repo.list(&scoped, 1, 2).await.expect("page 1");
    assert_eq!(result.total, 3);
    assert_eq!(result.items.len(), 2);
    assert_eq!(result.items[0].task_id, t3);
    assert_eq!(result.items[1].task_id, t2);
    let result = repo.list(&scoped, 2, 2).await.expect("page 2");
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].task_id, t1);

    // 清理本测试行,不污染共享库。
    for task_id in [&t1, &t2, &t3] {
        repo.update_terminal(task_id, "failed", now, None, None)
            .await
            .expect("cleanup terminal");
    }
}

/// userapp_metadata:upsert 刷新 name/tenant/space 但**不刷新 created_at**
/// （业务首次创建时间不可变）；load_all/delete roundtrip。
#[tokio::test]
async fn app_metadata_upsert_keeps_created_at_and_roundtrip() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&dsn)
        .await
        .expect("test pool");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrate");
    let repo = crate::pg::userapp::metadata::PgAppMetadataPersistence::new(pool);
    use shared_types::AppMetadataPersistence as _;
    let app_id = format!("meta-{}", uuid_suffix());
    let created_at = chrono::Utc::now() - chrono::Duration::hours(1);

    // 首次 upsert（create 语义）
    repo.upsert(&shared_types::AppMetadataRecord {
        app_id: app_id.clone(),
        name: Some("v1".into()),
        tenant_id: Some("tenant-1".into()),
        space_id: None,
        created_at,
    })
    .await
    .expect("upsert v1");
    // 二次 upsert（update 语义:name/space 刷新,created_at 传 now 但不得生效）
    repo.upsert(&shared_types::AppMetadataRecord {
        app_id: app_id.clone(),
        name: Some("v2".into()),
        tenant_id: None,
        space_id: Some("space-2".into()),
        created_at: chrono::Utc::now(),
    })
    .await
    .expect("upsert v2");

    let rows = repo.load_all().await.expect("load");
    let row = rows
        .iter()
        .find(|r| r.app_id == app_id)
        .expect("row loaded");
    assert_eq!(row.name.as_deref(), Some("v2"), "name refreshed by upsert");
    assert_eq!(row.space_id.as_deref(), Some("space-2"));
    assert!(
        row.tenant_id.is_none(),
        "update 未携带的租户被置空（整段替换语义）"
    );
    assert_eq!(
        row.created_at, created_at,
        "created_at must NOT be refreshed by upsert"
    );

    repo.delete(&app_id).await.expect("delete");
    let rows = repo.load_all().await.expect("reload");
    assert!(rows.iter().all(|r| r.app_id != app_id));
}

#[tokio::test]
async fn activity_persistence_roundtrip() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&dsn)
        .await
        .expect("test pool");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrate");
    let persistence = crate::pg::userapp::activity::PgActivityPersistence::new(pool.clone());
    use shared_types::ActivityPersistence as _;
    let app_id = format!("actapp-{}", uuid_suffix());

    // flush → load 往返
    let at = chrono::Utc::now() - chrono::Duration::hours(3);
    persistence
        .flush_batch(vec![shared_types::ActivityRow {
            app_id: app_id.clone(),
            last_accessed: Some(at),
            stopped: true,
            wake_blocked: false,
        }])
        .await
        .expect("flush");
    let loaded = persistence.load_all().await.expect("load");
    let row = loaded
        .iter()
        .find(|r| r.app_id == app_id)
        .expect("row loaded");
    assert!(row.stopped);
    assert!(!row.wake_blocked);
    assert_eq!(row.last_accessed, Some(at));

    // delete
    persistence.delete(&app_id).await.expect("delete");
    let loaded = persistence.load_all().await.expect("reload");
    assert!(loaded.iter().all(|r| r.app_id != app_id));
}
