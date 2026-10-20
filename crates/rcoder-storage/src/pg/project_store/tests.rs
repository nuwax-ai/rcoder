//! 主服务域 PG 集成测试（ProjectStore write-behind / 加载 / 跨副本同步 / 选主 / durable）。
//!
//! 共享 helper 见 `crate::pg::test_support`；Userapp 域测试在 `crate::pg::userapp::tests`。

#![cfg(all(test, feature = "pg"))]

use std::sync::Arc;
use std::time::Duration;

use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo, ProjectStore};

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
        workload_uid: None,
    }
}

fn pg_config(dsn: &str) -> PostgresConfig {
    PostgresConfig {
        url: Some(dsn.to_string()),
        ..PostgresConfig::default()
    }
}

#[tokio::test]
async fn shared_container_revision_does_not_reject_session_registration() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let suffix = uuid_suffix();
    let first_id = format!("shared-first-{suffix}");
    let second_id = format!("shared-second-{suffix}");
    let user_id = format!("shared-user-{suffix}");
    let first_session = format!("shared-session-first-{suffix}");
    let second_session = format!("shared-session-second-{suffix}");
    let mut container = container_for(&user_id);
    container.workload_uid = Some(format!("shared-workload-{suffix}"));
    let (store, _rx) = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .expect("connect disposable PG");
    let project = |id: &str| {
        let mut info = ProjectAndContainerInfo::new(id.to_owned());
        info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
        info.set_user_id(Some(user_id.clone()));
        info.set_container(Some(container.clone()));
        Arc::new(info)
    };
    store.insert(first_id.clone(), project(&first_id)).unwrap();
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    // Chat A captured its own project before chat B registered the same actual
    // shared container. Only the container's ordinary row revision advances.
    let first_snapshot = store.get(&first_id).unwrap();
    store
        .insert_with_session_durable(second_id.clone(), project(&second_id), &second_session)
        .await
        .unwrap();
    store
        .insert_with_session_durable(first_id.clone(), first_snapshot, &first_session)
        .await
        .expect("another project using the same physical container is not a replacement");
    assert_eq!(
        store
            .get_by_session_id(&first_session)
            .unwrap()
            .project_id(),
        first_id
    );
    assert_eq!(
        store
            .get_by_session_id(&second_session)
            .unwrap()
            .project_id(),
        second_id
    );
    assert!(store.wait_drained(Duration::from_secs(5)).await);
    let (peer, _rx) = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .unwrap();
    assert_eq!(
        peer.get_by_session_id(&first_session).unwrap().project_id(),
        first_id
    );
    assert_eq!(
        peer.get_by_session_id(&second_session)
            .unwrap()
            .project_id(),
        second_id
    );
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(store.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn peer_shared_container_revision_keeps_session_registration_durable() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let suffix = uuid_suffix();
    let first_id = format!("peer-shared-first-{suffix}");
    let second_id = format!("peer-shared-second-{suffix}");
    let user_id = format!("peer-shared-user-{suffix}");
    let first_session = format!("peer-shared-session-first-{suffix}");
    let second_session = format!("peer-shared-session-second-{suffix}");
    let mut container = container_for(&user_id);
    container.workload_uid = Some(format!("peer-shared-workload-{suffix}"));
    let project = |id: &str| {
        let mut info = ProjectAndContainerInfo::new(id.to_owned());
        info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
        info.set_user_id(Some(user_id.clone()));
        info.set_container(Some(container.clone()));
        Arc::new(info)
    };
    let (first, _rx) = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .unwrap();
    first.insert(first_id.clone(), project(&first_id)).unwrap();
    assert!(first.wait_drained(Duration::from_secs(5)).await);
    let first_snapshot = first.get(&first_id).unwrap();
    // The second replica has its own cursor. Its commit is deliberately not
    // synchronized into the first replica before chat A's completion arrives.
    let (second, _rx) =
        PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
            .await
            .unwrap();
    let mut peer_project = (*project(&second_id)).clone();
    let mut latest_container = container.clone();
    latest_container.service_url = format!("http://updated-shared-{suffix}");
    peer_project.set_container(Some(latest_container.clone()));
    second
        .insert_with_session_durable(second_id.clone(), Arc::new(peer_project), &second_session)
        .await
        .unwrap();
    assert!(second.wait_drained(Duration::from_secs(5)).await);
    first
        .insert_with_session_durable(first_id.clone(), first_snapshot, &first_session)
        .await
        .unwrap();
    assert!(first.wait_drained(Duration::from_secs(5)).await);
    let (reader, _rx) =
        PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
            .await
            .unwrap();
    assert_eq!(
        reader
            .get_by_session_id(&first_session)
            .expect("accepted chat A must be subscribable on a different replica immediately")
            .project_id(),
        first_id
    );
    assert_eq!(
        reader
            .get_by_session_id(&second_session)
            .unwrap()
            .project_id(),
        second_id
    );
    assert_eq!(
        reader
            .get(&first_id)
            .unwrap()
            .container_info()
            .unwrap()
            .service_url,
        latest_container.service_url,
        "a stale shared-container registration must preserve the peer's newer metadata"
    );
    assert!(reader.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(second.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(first.writer().flush_and_stop(Duration::from_secs(5)).await);
}

#[tokio::test]
async fn roundtrip_persists_and_reload_recovers() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let pool = crate::pg::test_support::database(&dsn).await;

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
                     AND agent_status_json IS NOT NULL AND container_name IS NOT NULL"
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
    let pool = crate::pg::test_support::database(&dsn).await;
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
    let pool_b = store_b.database.clone();

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

    // insert_with_session is write-behind; B's drain barrier cannot wait for A.
    assert!(
        store_a.wait_drained(Duration::from_secs(5)).await,
        "A insert drain"
    );
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
    let pool = store.database.clone();
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
    let _pool = crate::pg::test_support::database(&dsn).await;

    let (tx_a, _) = tokio::sync::broadcast::channel(1);
    let a = Arc::new(crate::pg::leader_selection::PgLeaderElection::spawn(
        PostgresConfig {
            url: Some(dsn.clone()),
            ..Default::default()
        },
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
        PostgresConfig {
            url: Some(dsn.clone()),
            ..Default::default()
        },
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
    let pool = crate::pg::test_support::database(&dsn).await;

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
    let lookup_session = session_id.clone();
    let fetched = super::database::read(&pool, move |tx| {
        Box::pin(async move { super::repo::fetch_project_by_session(tx, &lookup_session).await })
    })
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
    let _pool = crate::pg::test_support::database(&dsn).await;

    let project_id = format!("pgfetch-{}", uuid_suffix());
    let session_id = format!("sess-{project_id}");

    // B must load before A commits: startup now restores persisted session identity.
    // This creates a real cross-replica miss without weakening the miss assertion.
    let (store_b, _rx_b) =
        PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
            .await
            .expect("connect B");

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

/// A completed task is not proof that its final drain committed.
#[tokio::test]
async fn shutdown_failure_is_not_success_on_first_or_repeated_call() {
    use super::{persist_ops::PersistOp, writer::PersistWriter};
    use std::sync::atomic::AtomicI64;
    let pool = crate::db::owner::DatabaseOwner::closed_for_test();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tx.send(PersistOp::RemoveProject {
        project_id: "shutdown-contract".into(),
        generation: "old".into(),
    })
    .expect("queue operation");
    let writer = PersistWriter::spawn(pool, rx, Arc::new(AtomicI64::new(1)));
    assert!(
        !writer.flush_and_stop(Duration::from_secs(2)).await,
        "failed final drain must not report success"
    );
    assert!(
        !writer.flush_and_stop(Duration::from_secs(2)).await,
        "repeated shutdown must preserve failure"
    );
}

/// 契约三行3/§1.2 换代门（workload 身份语义）：同名 workload 下物理 UID
/// 变化=换代事务（新 generation+predecessor）；同名 workload 被替换（UID
/// 变化）= 拒绝自动重绑。容器名恒定（同一 workload 的注册键）。
#[tokio::test]
async fn workload_generation_gate_allows_rebind_only_for_same_workload() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let pool = crate::pg::test_support::database(&dsn).await;
    let project_id = format!("wrep-{}", uuid_suffix());
    let (store, _rx) = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .expect("connect");

    let container_with = |physical_uid: &str, workload_uid: &str| ContainerBasicInfo {
        container_id: physical_uid.into(),
        // 同一 workload 的注册键恒定（契约一：workload 名）
        container_name: "rcoder-web-wrep".into(),
        container_ip: "10.42.0.9".into(),
        internal_port: 50051,
        external_port: 0,
        project_id: project_id.clone(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        service_url: "http://rcoder-web-wrep".into(),
        workload_uid: Some(workload_uid.into()),
    };

    // 首次注册：pod-a 属 sts-uid-1。
    store
        .insert_with_session(
            project_id.clone(),
            info_for(&project_id, Some(container_with("pod-a", "sts-uid-1"))),
            Some(&format!("sess-a-{project_id}")),
        )
        .expect("first registration");
    assert!(
        store.wait_drained(Duration::from_secs(5)).await,
        "first registration drain"
    );
    let first = store.get(&project_id).expect("registered");
    let first_generation = first
        .persistence_identity()
        .container
        .as_ref()
        .expect("container identity")
        .generation
        .clone();

    // 同 workload（sts-uid-1）重建新 pod（pod-b）：换代事务放行——
    // 新 generation 落盘，注册绑定到新物理 UID。
    store
        .insert_with_session(
            project_id.clone(),
            info_for(&project_id, Some(container_with("pod-b", "sts-uid-1"))),
            Some(&format!("sess-b-{project_id}")),
        )
        .expect("same-workload pod replacement rebinds");
    assert!(
        store.wait_drained(Duration::from_secs(5)).await,
        "rebind drain"
    );
    let rebound = store.get(&project_id).expect("rebound");
    let rebound_identity = rebound
        .persistence_identity()
        .container
        .as_ref()
        .expect("rebound container identity");
    assert_eq!(
        rebound.container_info().expect("info").container_id,
        "pod-b"
    );
    assert_eq!(
        rebound_identity.physical_uid.as_deref(),
        Some("pod-b"),
        "identity carries the new physical UID"
    );
    assert_ne!(
        rebound_identity.generation, first_generation,
        "pod replacement must go through a new container generation, not an in-place revision"
    );
    assert_eq!(
        rebound_identity.predecessor.as_deref(),
        Some(first_generation.as_str()),
        "predecessor generation is captured atomically"
    );

    // 同名 workload 被替换（sts-uid-2）：拒绝自动重绑——行3 语义。
    let rejected = store.insert_with_session(
        project_id.clone(),
        info_for(&project_id, Some(container_with("pod-x", "sts-uid-2"))),
        Some(&format!("sess-c-{project_id}")),
    );
    assert!(
        rejected.is_err(),
        "a replaced workload (same name, different UID) must never be auto-rebound"
    );

    let _ = store.writer().flush_and_stop(Duration::from_secs(5)).await;
    // 函数末行显式释放连接池（`let _ = pool` 立即 drop 但触发
    // -W let-underscore-drop 告警，语义相同、意图更明确）。
    drop(pool);
}

/// 事故反例（2026-10-20 多副本 stale 绑定）：peer 副本 cleaner/stop 的
/// durable 删除（STS + PG 行 + tombstone）只清自己的内存；本副本镜像残留
/// 旧 workload UID，chat 落到本副本重建容器并 durable 注册时，权威门必须
/// 按 DB（行已删 + tombstone 在案）放行自我换代——修复前被残留镜像误判为
/// 外部替换，`/computer/chat` 永久 500。
#[tokio::test]
async fn authoritative_gate_allows_replacement_after_peer_delete() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let project_id = format!("wrep-peer-{}", uuid_suffix());
    let container_name = format!("rcoder-car-{project_id}");
    let mk = |physical: &str, workload: &str| ContainerBasicInfo {
        container_id: format!("{physical}-{project_id}"),
        container_name: container_name.clone(),
        container_ip: "10.42.0.9".into(),
        internal_port: 50051,
        external_port: 0,
        project_id: project_id.clone(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        service_url: format!("http://{container_name}"),
        workload_uid: Some(workload.into()),
    };
    let project_for = |container: ContainerBasicInfo| {
        let mut info = ProjectAndContainerInfo::new(project_id.clone());
        info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
        info.set_user_id(Some(format!("user-{project_id}")));
        info.set_container(Some(container));
        Arc::new(info)
    };

    let (store_a, _rx_a) =
        PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
            .await
            .expect("connect A");
    store_a
        .insert_project_registration_durable(
            project_id.clone(),
            project_for(mk("pod-a", "sts-uid-old")),
        )
        .await
        .expect("A registers the original binding");
    assert!(
        store_a.wait_drained(Duration::from_secs(5)).await,
        "A register drain"
    );

    // 副本 B 后连（镜像含旧 UID），随后 A durable 删除（写 tombstone）。
    let (store_b, _rx_b) =
        PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
            .await
            .expect("connect B");
    let (deleted, projects) = store_a
        .delete_container_with_projects_durable(&format!("pod-a-{project_id}"))
        .await;
    assert!(
        (deleted, projects) == (true, 1),
        "A durable delete: {deleted}/{projects}"
    );
    assert!(
        store_a.wait_drained(Duration::from_secs(5)).await,
        "A delete drain"
    );

    // B 不 sync、不重启，直接以重建后的新 workload durable 注册（事故路径）。
    let rebound = store_b
        .insert_project_registration_durable(
            project_id.clone(),
            project_for(mk("pod-b", "sts-uid-new")),
        )
        .await;
    assert!(
        rebound.is_ok(),
        "peer-deleted binding must re-register via the authoritative gate: {rebound:?}"
    );

    // DB 为事实源：新行重建、旧代次 tombstone 在案。
    let pool_b = store_b.database.clone();
    assert!(
        wait_for(
            &pool_b,
            &format!(
                "SELECT count(*) FROM containers WHERE container_name='{container_name}' AND container_id='pod-b-{project_id}'"
            ),
            1
        )
        .await,
        "new container row must be rebuilt"
    );
    assert!(
        wait_for(
            &pool_b,
            &format!(
                "SELECT count(*) FROM container_tombstones WHERE container_name='{container_name}'"
            ),
            1
        )
        .await,
        "the retired generation must keep its tombstone"
    );

    let _ = store_b
        .writer()
        .flush_and_stop(Duration::from_secs(5))
        .await;
    let _ = store_a
        .writer()
        .flush_and_stop(Duration::from_secs(5))
        .await;
}

/// Fix 2：peer 删除后 sync 必须失效本地容器注册表与镜像（DB 是唯一事实源），
/// 收敛后新 workload 注册成功。
#[tokio::test]
async fn cross_replica_sync_removes_stale_container_mirror() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let project_id = format!("wrep-sync-{}", uuid_suffix());
    let container_name = format!("rcoder-car-{project_id}");
    let mk = |physical: &str, workload: &str| ContainerBasicInfo {
        container_id: format!("{physical}-{project_id}"),
        container_name: container_name.clone(),
        container_ip: "10.42.0.9".into(),
        internal_port: 50051,
        external_port: 0,
        project_id: project_id.clone(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        service_url: format!("http://{container_name}"),
        workload_uid: Some(workload.into()),
    };
    let project_for = |container: ContainerBasicInfo| {
        let mut info = ProjectAndContainerInfo::new(project_id.clone());
        info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
        info.set_user_id(Some(format!("user-{project_id}")));
        info.set_container(Some(container));
        Arc::new(info)
    };

    let (store_a, _rx_a) =
        PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
            .await
            .expect("connect A");
    store_a
        .insert_project_registration_durable(
            project_id.clone(),
            project_for(mk("pod-a", "sts-uid-old")),
        )
        .await
        .expect("A registers the original binding");
    assert!(
        store_a.wait_drained(Duration::from_secs(5)).await,
        "A register drain"
    );

    let (store_b, _rx_b) =
        PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
            .await
            .expect("connect B");
    let (deleted, projects) = store_a
        .delete_container_with_projects_durable(&format!("pod-a-{project_id}"))
        .await;
    assert!(
        (deleted, projects) == (true, 1),
        "A durable delete: {deleted}/{projects}"
    );
    assert!(
        store_a.wait_drained(Duration::from_secs(5)).await,
        "A delete drain"
    );

    assert!(
        store_b
            .container_registration
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&container_name),
        "precondition: B mirror still holds the stale binding"
    );
    sync::sync_once(&store_b, store_b.inner(), &store_b.database)
        .await
        .expect("sync once");
    assert!(
        !store_b
            .container_registration
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(&container_name),
        "peer-deleted container must leave B's registration map"
    );
    assert!(
        !store_b
            .inner()
            .get_all_container_records()
            .iter()
            .any(|c| c.container_name == container_name),
        "peer-deleted container must leave B's adapter mirror"
    );

    let rebound = store_b
        .insert_project_registration_durable(
            project_id.clone(),
            project_for(mk("pod-b", "sts-uid-new")),
        )
        .await;
    assert!(
        rebound.is_ok(),
        "converged replica must accept the rebuilt workload: {rebound:?}"
    );

    let _ = store_b
        .writer()
        .flush_and_stop(Duration::from_secs(5))
        .await;
    let _ = store_a
        .writer()
        .flush_and_stop(Duration::from_secs(5))
        .await;
}

/// 守卫用例：DB 绑定仍在的 workload 替换（真外部替换）保持拒绝且类型化为
/// 409 冲突；行与 tombstone 俱无（绕过 durable 的手工删行）同样拒绝。
#[tokio::test]
async fn external_workload_replacement_still_refused_as_conflict() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let project_id = format!("wrep-ext-{}", uuid_suffix());
    let container_name = format!("rcoder-car-{project_id}");
    let mk = |physical: &str, workload: &str| ContainerBasicInfo {
        container_id: format!("{physical}-{project_id}"),
        container_name: container_name.clone(),
        container_ip: "10.42.0.9".into(),
        internal_port: 50051,
        external_port: 0,
        project_id: project_id.clone(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        service_url: format!("http://{container_name}"),
        workload_uid: Some(workload.into()),
    };
    let project_for = |container: ContainerBasicInfo| {
        let mut info = ProjectAndContainerInfo::new(project_id.clone());
        info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
        info.set_user_id(Some(format!("user-{project_id}")));
        info.set_container(Some(container));
        Arc::new(info)
    };

    let (store_a, _rx_a) =
        PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
            .await
            .expect("connect A");
    store_a
        .insert_project_registration_durable(
            project_id.clone(),
            project_for(mk("pod-a", "sts-uid-old")),
        )
        .await
        .expect("A registers the original binding");
    assert!(
        store_a.wait_drained(Duration::from_secs(5)).await,
        "A register drain"
    );

    let (store_b, _rx_b) =
        PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
            .await
            .expect("connect B");

    // 真外部替换：DB 行仍在（sts-uid-old），B 观察到 sts-uid-new → 拒绝。
    let refused = store_b
        .insert_project_registration_durable(
            project_id.clone(),
            project_for(mk("pod-b", "sts-uid-new")),
        )
        .await
        .expect_err("an externally replaced workload must never be auto-rebound");
    let typed = refused
        .downcast_ref::<shared_types::ContainerWorkloadReplaced>()
        .expect("the refusal is the typed 409 conflict");
    assert_eq!(typed.container_name, container_name);
    assert_eq!(typed.registered_workload_uid, "sts-uid-old");
    assert_eq!(typed.observed_workload_uid, "sts-uid-new");
    assert_eq!(
        store_b
            .container_registration
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&container_name)
            .and_then(|identity| identity.physical_uid.as_deref()),
        Some(format!("pod-a-{project_id}").as_str()),
        "refusal must not mutate the local registration"
    );

    // 异常态：绕过 durable 直接删行（无 tombstone）→ 同样拒绝，不静默放行。
    let owner = store_a.database.clone();
    let raw_name = container_name.clone();
    owner
        .execute(move |mut db| async move {
            // 模拟手工动库：绕过 durable 路径直接删行（无 tombstone）。
            toasty::sql::query("DELETE FROM projects WHERE container_name=$1")
                .bind(raw_name.as_str())
                .exec(&mut db)
                .await?;
            toasty::sql::query("DELETE FROM containers WHERE container_name=$1")
                .bind(raw_name)
                .exec(&mut db)
                .await?;
            Ok(Vec::<toasty_core::stmt::Value>::new())
        })
        .await
        .expect("raw row delete");
    let anomalous = store_b
        .insert_project_registration_durable(
            project_id.clone(),
            project_for(mk("pod-c", "sts-uid-newer")),
        )
        .await
        .expect_err("a vanished binding without a tombstone must stay refused");
    assert!(
        anomalous
            .to_string()
            .contains("vanished without a retirement record"),
        "anomalous absence keeps an explicit refusal, got: {anomalous:#}"
    );

    let _ = store_b
        .writer()
        .flush_and_stop(Duration::from_secs(5))
        .await;
    let _ = store_a
        .writer()
        .flush_and_stop(Duration::from_secs(5))
        .await;
}
