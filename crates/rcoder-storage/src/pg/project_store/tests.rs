//! 主服务域 PG 集成测试（ProjectStore write-behind / 加载 / 跨副本同步 / 选主 / durable）。
//!
//! 共享 helper 见 `crate::pg::test_support`；Userapp 域测试在 `crate::pg::userapp::tests`。

#![cfg(all(test, feature = "pg"))]

use std::sync::Arc;
use std::time::Duration;

use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo, ProjectStore};
use sqlx::postgres::PgPoolOptions;

use crate::config::PostgresConfig;
use crate::pg::test_support::{DSN_ENV, test_dsn, uuid_suffix, wait_for};
use crate::pg::{PgStore, sync};

fn info_for(
    project_id: &str,
    container: Option<ContainerBasicInfo>,
) -> Arc<ProjectAndContainerInfo> {
    let mut info = ProjectAndContainerInfo::new(project_id.to_string());
    info.set_service_type(Some(shared_types::ServiceType::WebAgentRunner));
    info.set_user_id(Some(format!("user-{project_id}")));
    if let Some(c) = container {
        info.set_container(Some(c));
    }
    Arc::new(info)
}

fn container_for(project_id: &str) -> ContainerBasicInfo {
    ContainerBasicInfo {
        container_id: format!("cid-{project_id}"),
        container_name: format!("container-{project_id}"),
        container_ip: "10.42.0.9".into(),
        internal_port: 50051,
        external_port: 0,
        project_id: project_id.into(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        service_url: format!("http://container-{project_id}"),
    }
}

fn pg_config(dsn: &str) -> PostgresConfig {
    PostgresConfig {
        url: Some(dsn.to_string()),
        ..PostgresConfig::default()
    }
}

#[tokio::test]
async fn roundtrip_persists_and_reload_recovers() {
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

    let project_id = format!("pgtest-{}", uuid_suffix());
    let session_id = format!("sess-{project_id}");

    // 第一段：写入 → write-behind 落 PG
    {
        let (store, _rx) =
            PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
                .await
                .expect("connect");
        store
            .insert_with_session(
                project_id.clone(),
                info_for(&project_id, Some(container_for(&project_id))),
                Some(&session_id),
            )
            .expect("insert_with_session");
        store.update_agent_status(&project_id, 1, "Active");

        assert!(
            wait_for(
                &pool,
                &format!(
                    "SELECT count(*) FROM sessions WHERE session_id='{session_id}' \
                     AND project_id='{project_id}'"
                ),
                1
            )
            .await,
            "session row must persist"
        );
        assert!(
            wait_for(
                &pool,
                &format!(
                    "SELECT count(*) FROM projects WHERE project_id='{project_id}' \
                     AND agent_status IS NOT NULL AND container_name IS NOT NULL"
                ),
                1
            )
            .await,
            "project row (with agent_status + container ref) must persist"
        );
        assert!(
            wait_for(
                &pool,
                &format!("SELECT count(*) FROM containers WHERE container_id='cid-{project_id}'"),
                1
            )
            .await,
            "container row must persist"
        );
        // 优雅关停 flush（顺带验证 flush_and_stop 语义）
        assert!(
            store.writer().flush_and_stop(Duration::from_secs(5)).await,
            "writer flush must succeed"
        );
    }

    // 第二段：模拟重启——drop 旧 store（连接释放），重连全量加载
    {
        let (store2, _rx) =
            PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
                .await
                .expect("reconnect");
        let resolved = store2
            .get_by_session_id(&session_id)
            .expect("session resolve from loaded mirror");
        assert_eq!(resolved.project_id(), project_id);
        assert!(store2.get_container_name_by_session(&session_id).is_some());
        let _ = store2.writer().flush_and_stop(Duration::from_secs(5)).await;
    }

    // 清理：删除 project（级联 session）
    {
        let (store3, _rx) =
            PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
                .await
                .expect("connect for cleanup");
        assert!(store3.remove(&project_id).is_some());
        assert!(
            wait_for(
                &pool,
                &format!("SELECT count(*) FROM projects WHERE project_id='{project_id}'"),
                0
            )
            .await,
            "project row must be deleted"
        );
        assert!(
            wait_for(
                &pool,
                &format!("SELECT count(*) FROM sessions WHERE session_id='{session_id}'"),
                0
            )
            .await,
            "session rows must cascade-delete"
        );
        let _ = store3.writer().flush_and_stop(Duration::from_secs(5)).await;
    }
}

#[tokio::test]
async fn clear_sessions_and_delete_container() {
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
    let project_id = format!("pgdel-{}", uuid_suffix());

    let (store, _rx) = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .expect("connect");
    store
        .insert_with_session(
            project_id.clone(),
            info_for(&project_id, Some(container_for(&project_id))),
            Some(&format!("sess-a-{project_id}")),
        )
        .expect("insert");
    assert!(store.add_session_to_project(&project_id, &format!("sess-b-{project_id}")));

    // clear_session：全部 session 清空
    store.clear_session(&project_id);
    assert!(
        wait_for(
            &pool,
            &format!("SELECT count(*) FROM sessions WHERE project_id='{project_id}'"),
            0
        )
        .await,
        "sessions must be cleared"
    );

    // delete_container_with_projects：project + container 行全删
    let (deleted, count) = store.delete_container_with_projects(&format!("cid-{project_id}"));
    assert!(deleted, "container record must exist");
    assert_eq!(count, 1, "one project removed with container");
    assert!(
        wait_for(
            &pool,
            &format!("SELECT count(*) FROM projects WHERE project_id='{project_id}'"),
            0
        )
        .await,
        "project row must be deleted"
    );
    assert!(
        wait_for(
            &pool,
            &format!("SELECT count(*) FROM containers WHERE container_id='cid-{project_id}'"),
            0
        )
        .await,
        "container row must be deleted"
    );
    let _ = store.writer().flush_and_stop(Duration::from_secs(5)).await;
}

/// 短随机后缀（测试间隔离；不引入 uuid 依赖）

#[tokio::test]
async fn cross_replica_sync_visibility_and_removal() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let pg_config = |dsn: &str| PostgresConfig {
        url: Some(dsn.to_string()),
        ..PostgresConfig::default()
    };

    // 副本 B 先连（空库快照），模拟已运行的旧副本
    let (store_b, _rx_b) =
        PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
            .await
            .expect("connect B");
    let pool_b = store_b.pool().clone();

    // 副本 A 连接并写入
    let project_id = format!("xrep-{}", uuid_suffix());
    let session_id = format!("sess-{project_id}");
    let (store_a, _rx_a) =
        PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
            .await
            .expect("connect A");
    store_a
        .insert_with_session(
            project_id.clone(),
            info_for(&project_id, Some(container_for(&project_id))),
            Some(&session_id),
        )
        .expect("A insert");

    // B 尚未同步 → 不可见（模拟 ClientIP affinity 下的另一副本）
    assert!(store_b.get_by_session_id(&session_id).is_none());

    // B 同步 → 可见（session resolve + 容器反查）
    sync::sync_once(&store_b, store_b.inner(), &pool_b)
        .await
        .expect("sync once");
    assert!(
        store_b.get_by_session_id(&session_id).is_some(),
        "B must see A's session after sync"
    );
    assert_eq!(
        store_b
            .get_container_name_by_session(&session_id)
            .as_deref(),
        Some(container_for(&project_id).container_name.as_str())
    );

    // A 删除 → B 同步后移除
    assert!(store_a.remove(&project_id).is_some());
    assert!(
        store_a.wait_drained(Duration::from_secs(5)).await,
        "A drain"
    );
    sync::sync_once(&store_b, store_b.inner(), &pool_b)
        .await
        .expect("sync once 2");
    assert!(
        store_b.get_by_session_id(&session_id).is_none(),
        "B must drop removed session after sync"
    );
    assert!(!store_b.contains_key(&project_id));
    let _ = store_b
        .writer()
        .flush_and_stop(Duration::from_secs(5))
        .await;
    let _ = store_a
        .writer()
        .flush_and_stop(Duration::from_secs(5))
        .await;
}

/// P2-M1：sync 幂等——本副本已落库的条目经 sync 不丢、不重复（屏障正确性）。
/// 注：断言只针对本测试自己的条目（PG 为共享库，并行测试会增减全局行数）。
#[tokio::test]
async fn cross_replica_sync_is_idempotent_for_own_entries() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let (store, _rx) = PgStore::connect(
        &PostgresConfig {
            url: Some(dsn),
            ..PostgresConfig::default()
        },
        "test-ns".into(),
        "cluster.local".into(),
    )
    .await
    .expect("connect");
    let pool = store.pool().clone();
    let project_id = format!("synco-{}", uuid_suffix());
    let session_id = format!("sess-{project_id}");
    store
        .insert_with_session(
            project_id.clone(),
            info_for(&project_id, Some(container_for(&project_id))),
            Some(&session_id),
        )
        .expect("insert");
    assert!(
        store.wait_drained(Duration::from_secs(5)).await,
        "drain before sync"
    );

    // 连续两轮 sync：自己的条目仍在、session 仍可解析（不丢不重）
    for _ in 0..2 {
        sync::sync_once(&store, store.inner(), &pool)
            .await
            .expect("sync");
        assert!(
            store.contains_key(&project_id),
            "own project must survive sync"
        );
        assert!(store.get_by_session_id(&session_id).is_some());
    }
    // 清理
    assert!(store.remove(&project_id).is_some());
    let _ = store.wait_drained(Duration::from_secs(5)).await;
    let _ = store.writer().flush_and_stop(Duration::from_secs(5)).await;
}

/// P2-M3：leader 互斥——两个选举实例指向同一 PG，同时刻至多一个 leader。
/// （让位时延为 5s 轮询，测试只验证互斥不变式；故障切换由保活探测路径覆盖，
/// 连接死亡场景在集成环境验证。）
#[tokio::test]
async fn leader_election_mutual_exclusion() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&dsn)
        .await
        .expect("pool");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrate");

    let (tx_a, _) = tokio::sync::broadcast::channel(1);
    let a = Arc::new(crate::pg::leader_selection::PgLeaderElection::spawn(
        pool.clone(),
        tx_a.subscribe(),
    ));
    // 等抢锁窗口（poll 5s → 最多等 12s）
    let mut leader_seen = false;
    for _ in 0..24 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if a.is_leader() {
            leader_seen = true;
            break;
        }
    }
    assert!(
        leader_seen,
        "A should acquire leadership within poll window"
    );

    let (tx_b, _) = tokio::sync::broadcast::channel(1);
    let b = Arc::new(crate::pg::leader_selection::PgLeaderElection::spawn(
        pool.clone(),
        tx_b.subscribe(),
    ));
    // B 观察一个完整窗口：不得成为 leader
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(
        !b.is_leader(),
        "B must NOT hold leadership while A holds the lock"
    );
    assert!(a.is_leader());
}

#[tokio::test]
async fn durable_commit_returns_after_pg_visible() {
    // durable 契约：方法返回（Ok）后，另起连接直查 PG 必然可见——
    // "chat 返回 session_id = 任何副本回源直查必命中"的存储层基础
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

    let project_id = format!("pgdur-{}", uuid_suffix());
    let session_id = format!("sess-{project_id}");
    let (store, _rx) = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .expect("connect");

    store
        .insert_with_session_durable(
            project_id.clone(),
            info_for(&project_id, Some(container_for(&project_id))),
            &session_id,
        )
        .await
        .expect("durable insert");

    // 另起连接直查（不经本 store 的内存/队列）——durable 返回即已提交
    let fetched = super::repo::fetch_project_by_session(&pool, &session_id)
        .await
        .expect("durable query ok")
        .expect("durable committed row must be visible immediately");
    assert_eq!(fetched.0.project_id, project_id);

    // 内存镜像同步可见
    assert!(store.get_by_session_id(&session_id).is_some());
}

#[tokio::test]
async fn session_miss_backfills_from_pg_into_mirror() {
    // 回源直查：镜像 miss → PG 查 → hydrate 进镜像（下次走内存命中）
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

    let project_id = format!("pgfetch-{}", uuid_suffix());
    let session_id = format!("sess-{project_id}");

    // 副本 A durable 写入（模拟 chat 落 A）
    let (store_a, _rx_a) =
        PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
            .await
            .expect("connect A");
    store_a
        .insert_with_session_durable(
            project_id.clone(),
            info_for(&project_id, Some(container_for(&project_id))),
            &session_id,
        )
        .await
        .expect("A durable insert");

    // 副本 B（镜像空，模拟另一 rcoder 副本）——回源直查必中 + hydrate
    let (store_b, _rx_b) =
        PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
            .await
            .expect("connect B");
    assert!(
        store_b.inner().get_by_session_id(&session_id).is_none(),
        "B mirror starts empty"
    );
    let fetched = store_b
        .get_by_session_id_with_fetch(&session_id)
        .await
        .expect("B backfill from PG");
    assert_eq!(fetched.project_id(), project_id);
    // hydrate 后镜像命中（不再回源）
    assert!(store_b.inner().get_by_session_id(&session_id).is_some());
    // 未知 session 回源仍 miss
    assert!(
        store_b
            .get_by_session_id_with_fetch("sess-nonexistent")
            .await
            .is_none()
    );
}
