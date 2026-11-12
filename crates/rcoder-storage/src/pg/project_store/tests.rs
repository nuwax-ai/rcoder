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

#[path = "authoritative_tests.rs"]
mod authoritative_tests;

#[path = "sync_retirement_tests.rs"]
mod sync_retirement_tests;

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
    let store = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
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
    let peer = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
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
    let first = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .unwrap();
    first.insert(first_id.clone(), project(&first_id)).unwrap();
    assert!(first.wait_drained(Duration::from_secs(5)).await);
    let first_snapshot = first.get(&first_id).unwrap();
    // The second replica has its own cursor. Its commit is deliberately not
    // synchronized into the first replica before chat A's completion arrives.
    let second = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
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
    let reader = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
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
        let store = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
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
        let store2 = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
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
        let store3 = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
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

    let store = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
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
    let store_b = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .expect("connect B");
    let pool_b = store_b.database.clone();

    // 副本 A 连接并写入
    let project_id = format!("xrep-{}", uuid_suffix());
    let session_id = format!("sess-{project_id}");
    let store_a = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
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
    let store = PgStore::connect(
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
    let store = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
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
    let store_b = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .expect("connect B");

    // 副本 A durable 写入（模拟 chat 落 A）
    let store_a = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
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
    let store = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
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

    let store_a = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
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
    let store_b = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
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
    assert_eq!(
        rebound.expect("peer-deleted binding must re-register via the authoritative gate"),
        shared_types::persistence::PersistenceWriteOutcome::Committed,
        "the first registration must be confirmed before dispatch"
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

    let store_a = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
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

    let store_b = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
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
    assert_eq!(
        rebound.expect("converged replica must accept the rebuilt workload"),
        shared_types::persistence::PersistenceWriteOutcome::Committed
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

/// 守卫用例（`RCODER_PG_BINDING_AUTOHEAL=false` 的回退行为）：
/// DB 绑定仍在的 workload 替换（真外部替换）保持拒绝且类型化为
/// 409 冲突；行与 tombstone 俱无（绕过 durable 的手工删行）同样拒绝。
#[tokio::test]
async fn workload_replacement_and_anomalous_absence_stay_refused_when_autoheal_disabled() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let config = || PostgresConfig {
        url: Some(dsn.clone()),
        binding_autoheal: false,
        ..PostgresConfig::default()
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

    let store_a = PgStore::connect(&config(), "test-ns".into(), "cluster.local".into())
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

    let store_b = PgStore::connect(&config(), "test-ns".into(), "cluster.local".into())
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
    sync::sync_once(&store_b, store_b.inner(), &store_b.database)
        .await
        .expect("sync preserves an anomalous container cursor");
    assert!(
        store_b
            .container_registration
            .lock()
            .unwrap()
            .contains_key(&container_name),
        "sync must not erase a missing binding without its retirement tombstone"
    );
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

// ---------------------------------------------------------------------------
// 绑定门禁自愈（binding_autoheal）
// ---------------------------------------------------------------------------

/// 读 DB 中容器绑定行：(container_id, workload_uid)。
async fn fetch_binding_row(
    store: &PgStore,
    name: &str,
) -> Option<(Option<String>, Option<String>)> {
    let name = name.to_owned();
    super::database::read(&store.database, move |tx| {
        Box::pin(async move {
            Ok(super::repo::fetch_container_registration(tx, &name)
                .await?
                .map(|row| (row.container_id, row.workload_uid)))
        })
    })
    .await
    .expect("read binding row")
}

/// 精确三元组 (name, generation, physical_uid) 的退休墓碑是否在案。
/// 断言自愈墓碑只按 (name, generation) 存在，且 `physical_uid` 为 NULL。
///
/// 这是复审 P1-1 的安全性要求：镜像旧 identity 不能证明物理对象已退役。
/// 一旦墓碑带上 physical_uid，`repo/containers.rs` 的
/// `physical_uid=$3` 分支会让该 UID 之后**任何**注册被判 Superseded，
/// 且墓碑不会自动消失 —— 属于永久性生产故障。
async fn assert_generation_only_tombstone(store: &PgStore, name: &str, generation: &str) {
    let sql = format!(
        "SELECT count(*) FROM container_tombstones \
         WHERE container_name='{name}' AND container_generation='{generation}' \
           AND physical_uid IS NULL"
    );
    assert!(
        wait_for(&store.database, &sql, 1).await,
        "自愈墓碑必须按代次存在且不带 physical_uid(否则该 UID 会被永久拒绝): {sql}"
    );
}

async fn exact_tombstone_exists(
    store: &PgStore,
    name: &str,
    generation: &str,
    physical_uid: &str,
) -> bool {
    let name = name.to_owned();
    let generation = generation.to_owned();
    let physical_uid = physical_uid.to_owned();
    super::database::read(&store.database, move |tx| {
        Box::pin(async move {
            super::repo::container_generation_is_retired(tx, &name, &generation, &physical_uid)
                .await
        })
    })
    .await
    .expect("read tombstone")
}

struct HealFixture {
    project_id: String,
    container_name: String,
    store_a: PgStore,
    store_b: PgStore,
    /// 同为旧游标的第二副本（连接于删行之前），用于多副本竞态用例。
    store_c: PgStore,
}

/// 异常复现骨架：A 注册原绑定 → B 连接（镜像持有旧游标）→ 绕过 durable
/// 裸删行（行+墓碑俱无 = AnomalousAbsent）。
async fn heal_fixture(physical: &str, workload: &str) -> Option<HealFixture> {
    let dsn = test_dsn().await?;
    let project_id = format!("heal-{}", uuid_suffix());
    let container_name = format!("rcoder-heal-{project_id}");
    let mk = |physical: &str, workload: &str| ContainerBasicInfo {
        container_id: if physical.is_empty() {
            String::new()
        } else {
            format!("{physical}-{project_id}")
        },
        container_name: container_name.clone(),
        container_ip: "10.42.0.9".into(),
        internal_port: 50051,
        external_port: 0,
        project_id: project_id.clone(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        service_url: format!("http://{container_name}"),
        workload_uid: (!workload.is_empty()).then(|| workload.to_string()),
    };
    let project_for = |container: ContainerBasicInfo| {
        let mut info = ProjectAndContainerInfo::new(project_id.clone());
        info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
        info.set_user_id(Some(format!("user-{project_id}")));
        info.set_container(Some(container));
        Arc::new(info)
    };
    let store_a = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .expect("connect A");
    store_a
        .insert_project_registration_durable(
            project_id.clone(),
            project_for(mk(physical, workload)),
        )
        .await
        .expect("A registers the original binding");
    assert!(
        store_a.wait_drained(Duration::from_secs(5)).await,
        "A register drain"
    );
    let store_b = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .expect("connect B");
    let store_c = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .expect("connect C");
    // 模拟手工动库/事务丢失：绕过 durable 路径直接删行（无 tombstone）。
    let owner = store_a.database.clone();
    let raw_name = container_name.clone();
    owner
        .execute(move |mut db| async move {
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
    Some(HealFixture {
        project_id,
        container_name,
        store_a,
        store_b,
        store_c,
    })
}

impl HealFixture {
    fn mk(&self, physical: &str, workload: &str) -> ContainerBasicInfo {
        ContainerBasicInfo {
            container_id: format!("{physical}-{}", self.project_id),
            container_name: self.container_name.clone(),
            container_ip: "10.42.0.9".into(),
            internal_port: 50051,
            external_port: 0,
            project_id: self.project_id.clone(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: format!("http://{}", self.container_name),
            workload_uid: Some(workload.into()),
        }
    }

    fn project_for(&self, container: ContainerBasicInfo) -> Arc<ProjectAndContainerInfo> {
        let mut info = ProjectAndContainerInfo::new(self.project_id.clone());
        info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
        info.set_user_id(Some(format!("user-{}", self.project_id)));
        info.set_container(Some(container));
        Arc::new(info)
    }

    /// B 镜像中残留的旧游标（generation, physical_uid）。
    fn stale_cursor(&self) -> (String, Option<String>) {
        let identity = self
            .store_b
            .container_registration
            .lock()
            .unwrap()
            .get(&self.container_name)
            .cloned()
            .expect("B still holds the stale cursor");
        (identity.generation, identity.physical_uid)
    }

    async fn finish(self) {
        let _ = self
            .store_c
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await;
        let _ = self
            .store_b
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await;
        let _ = self
            .store_a
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await;
    }
}

/// 路径 A 自愈（异常形态正片）：行+墓碑俱无时，持有完整旧
/// identity 的副本注册 → 补写墓碑 → 按自我换代放行，绑定行重建。
#[tokio::test]
async fn anomalous_absent_binding_heals_from_the_mirror_cursor() {
    let Some(f) = heal_fixture("pod-a", "sts-uid-old").await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let (stale_generation, stale_uid) = f.stale_cursor();
    let outcome = f
        .store_b
        .insert_project_registration_durable(
            f.project_id.clone(),
            f.project_for(f.mk("pod-c", "sts-uid-newer")),
        )
        .await
        .expect("the healed registration must be admitted");
    assert_eq!(
        outcome,
        shared_types::persistence::PersistenceWriteOutcome::Committed
    );
    assert!(
        f.store_b.wait_drained(Duration::from_secs(5)).await,
        "healed register drain"
    );
    // 自愈只按 (name, generation) 立墓碑，**不写 physical_uid**：
    // 镜像游标不能证明物理对象退役，写 uid 会让该 UID 之后任何注册被永久拒绝。
    let _ = &stale_uid;
    assert_generation_only_tombstone(&f.store_b, &f.container_name, &stale_generation).await;
    let row = fetch_binding_row(&f.store_b, &f.container_name)
        .await
        .expect("the binding row must be rebuilt");
    assert_eq!(
        row.0.as_deref(),
        Some(format!("pod-c-{}", f.project_id).as_str())
    );
    assert_eq!(row.1.as_deref(), Some("sts-uid-newer"));
    f.finish().await;
}

/// 路径 A 守卫：镜像旧游标缺 physical_uid（Docker 族残缺 identity）→
/// 无法构成墓碑三元组 → 读取保持 AnomalousAbsent、不写墓碑。
/// （注册层该形态走 ordinary rebase 分支，根本不进 §1.2 门，故直接断言
/// 读函数的自愈守卫。）
#[tokio::test]
async fn incomplete_mirror_identity_keeps_the_refusal() {
    let Some(f) = heal_fixture("", "").await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let (stale_generation, stale_uid) = f.stale_cursor();
    assert!(
        stale_uid.is_none(),
        "fixture must hold an incomplete cursor"
    );
    let read = f
        .store_b
        .read_authoritative_container_binding(
            &f.container_name,
            &f.project_id,
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("the authoritative read itself must succeed");
    assert_eq!(
        read.binding,
        super::authoritative::AuthoritativeContainerBinding::AnomalousAbsent,
        "an incomplete cursor must not be healed into RetiredByUs"
    );
    assert!(
        !exact_tombstone_exists(&f.store_b, &f.container_name, &stale_generation, "",).await,
        "no tombstone may be fabricated from an incomplete identity"
    );
    f.finish().await;
}

/// 路径 A 守卫：同 (name, generation) 已被不同 physical_uid 的墓碑占用 →
/// INSERT no-op 且三元组不精确成立 → 不自愈，保持原拒绝。
#[tokio::test]
async fn conflicting_tombstone_blocks_the_autoheal() {
    let Some(f) = heal_fixture("pod-a", "sts-uid-old").await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let (stale_generation, _) = f.stale_cursor();
    let owner = f.store_a.database.clone();
    let raw_name = f.container_name.clone();
    let raw_generation = stale_generation.clone();
    owner
        .execute(move |mut db| async move {
            toasty::sql::statement(
                "INSERT INTO container_tombstones(container_name,container_generation,physical_uid,retired_at_us) \
                 VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING",
            )
            .bind(&raw_name)
            .bind(&raw_generation)
            .bind("conflicting-physical-uid")
            .bind(chrono::Utc::now().timestamp_micros())
            .exec(&mut db)
            .await?;
            Ok(Vec::<toasty_core::stmt::Value>::new())
        })
        .await
        .expect("pre-seed a conflicting tombstone");
    let refused = f
        .store_b
        .insert_project_registration_durable(
            f.project_id.clone(),
            f.project_for(f.mk("pod-c", "sts-uid-newer")),
        )
        .await
        .expect_err("a conflicting tombstone must keep the refusal");
    assert!(
        refused
            .to_string()
            .contains("vanished without a retirement record"),
        "conflicting tombstone keeps an explicit refusal, got: {refused:#}"
    );
    f.finish().await;
}

/// 多副本竞态：B（旧游标）自愈重建行后，同为旧游标的 C 注册新 pod ——
/// 权威快照 reconcile 直接采纳 peer 重建的行（游标对齐 + RetiredByUs 对旧代
/// 放行），同一次注册内完成换代提交；两代墓碑（B 的自愈墓碑 + C 替换 peer
/// 行的墓碑）均在案可审计。
#[tokio::test]
async fn second_stale_replica_converges_after_a_peer_heal() {
    let Some(f) = heal_fixture("pod-a", "sts-uid-old").await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let (stale_generation, stale_uid) = f.stale_cursor();
    // B（旧游标）自愈重建
    let healed = f
        .store_b
        .insert_project_registration_durable(
            f.project_id.clone(),
            f.project_for(f.mk("pod-c", "sts-uid-newer")),
        )
        .await
        .expect("peer heal admits the registration");
    assert_eq!(
        healed,
        shared_types::persistence::PersistenceWriteOutcome::Committed
    );
    assert!(f.store_b.wait_drained(Duration::from_secs(5)).await);
    let peer_row = fetch_binding_row(&f.store_c, &f.container_name)
        .await
        .expect("the healed row must exist");
    assert_eq!(
        peer_row.0.as_deref(),
        Some(format!("pod-c-{}", f.project_id).as_str())
    );
    // C（同样持旧游标）：一次注册内完成采纳 + 换代
    let converged = f
        .store_c
        .insert_project_registration_durable(
            f.project_id.clone(),
            f.project_for(f.mk("pod-d", "sts-uid-newest")),
        )
        .await
        .expect("a stale replica converges onto the peer-rebuilt row");
    assert_eq!(
        converged,
        shared_types::persistence::PersistenceWriteOutcome::Committed
    );
    assert!(f.store_c.wait_drained(Duration::from_secs(5)).await);
    let row = fetch_binding_row(&f.store_c, &f.container_name)
        .await
        .expect("row must exist");
    assert_eq!(
        row.0.as_deref(),
        Some(format!("pod-d-{}", f.project_id).as_str())
    );
    assert_eq!(row.1.as_deref(), Some("sts-uid-newest"));
    // B 自愈墓碑（旧代）在案
    assert!(
        exact_tombstone_exists(
            &f.store_c,
            &f.container_name,
            &stale_generation,
            stale_uid.as_deref().unwrap_or_default(),
        )
        .await
    );
    f.finish().await;
}

/// sync 收敛：删行后 sync 保留异常游标（既有语义）；精确墓碑在案后 sync
/// 即清除游标（`self_confirm_container_alive` 按 tombstone 判非存活）。
#[tokio::test]
async fn sync_prunes_the_stale_cursor_once_the_exact_tombstone_exists() {
    let Some(f) = heal_fixture("pod-a", "sts-uid-old").await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    sync::sync_once(&f.store_b, f.store_b.inner(), &f.store_b.database)
        .await
        .expect("sync tolerates the anomalous cursor");
    assert!(
        f.store_b
            .container_registration
            .lock()
            .unwrap()
            .contains_key(&f.container_name),
        "sync must keep the cursor while no tombstone proves retirement"
    );
    let (stale_generation, stale_uid) = f.stale_cursor();
    let owner = f.store_a.database.clone();
    let raw_name = f.container_name.clone();
    owner
        .execute(move |mut db| async move {
            toasty::sql::statement(
                "INSERT INTO container_tombstones(container_name,container_generation,physical_uid,retired_at_us) \
                 VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING",
            )
            .bind(&raw_name)
            .bind(&stale_generation)
            .bind_typed(
                stale_uid.as_deref(),
                toasty_core::schema::db::Type::Text,
            )
            .bind(chrono::Utc::now().timestamp_micros())
            .exec(&mut db)
            .await?;
            Ok(Vec::<toasty_core::stmt::Value>::new())
        })
        .await
        .expect("record the exact tombstone");
    sync::sync_once(&f.store_b, f.store_b.inner(), &f.store_b.database)
        .await
        .expect("sync reconciles the retired cursor");
    assert!(
        !f.store_b
            .container_registration
            .lock()
            .unwrap()
            .contains_key(&f.container_name),
        "sync must prune the cursor once its exact tombstone is on record"
    );
    f.finish().await;
}

/// 路径 B 自愈（带外 workload 换代）：DB 行仍在但 pod UID/workload 已换
/// （kubectl delete pod / 节点宕机重建）→ 换代放行：旧代写墓碑、新代落库。
#[tokio::test]
async fn out_of_band_workload_replacement_retires_and_rebinds() {
    let Some(f) = heal_fixture("pod-a", "sts-uid-old").await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let (stale_generation, stale_uid) = f.stale_cursor();
    let outcome = f
        .store_b
        .insert_project_registration_durable(
            f.project_id.clone(),
            f.project_for(f.mk("pod-b", "sts-uid-new")),
        )
        .await
        .expect("an out-of-band replacement must be admitted under autoheal");
    assert_eq!(
        outcome,
        shared_types::persistence::PersistenceWriteOutcome::Committed
    );
    assert!(
        f.store_b.wait_drained(Duration::from_secs(5)).await,
        "replacement drain"
    );
    assert!(
        exact_tombstone_exists(
            &f.store_b,
            &f.container_name,
            &stale_generation,
            stale_uid.as_deref().unwrap_or_default(),
        )
        .await,
        "the replaced generation must leave a tombstone for audit"
    );
    let row = fetch_binding_row(&f.store_b, &f.container_name)
        .await
        .expect("the binding row must survive the replacement");
    assert_eq!(
        row.0.as_deref(),
        Some(format!("pod-b-{}", f.project_id).as_str())
    );
    assert_eq!(row.1.as_deref(), Some("sts-uid-new"));
    f.finish().await;
}

/// 审查 P1 反例：迟到登记的旧观察不得退休较新的绑定（Bound 行仍在分支）。
///
/// 时序：A 副本注册 workload-0 → B（创建更晚）带外换代 Committed（合法，
/// 同时补上"Bound 行仍在"的换代覆盖——原 out_of_band 用例走的是先删行的
/// heal_fixture）→ 迟到登记更旧的 A 观察：必须拒绝、绑定仍是 B、B 的
/// physical_uid 不进墓碑、B 可重新登记。
///
/// 修复前行为：迟到 A 被当作带外换代放行（UID 不同即放行），绑定翻成 A、
/// B 落物理退休墓碑，B 的后续登记永远 Superseded——真实新对象被旧观察封死。
#[tokio::test]
async fn late_older_observation_cannot_retire_newer_binding() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let id = format!("late-obs-{}", uuid_suffix());
    let container_name = format!("rcoder-{id}");
    let t0 = chrono::Utc::now();
    let mk = |physical: &str, workload: &str, created: chrono::DateTime<chrono::Utc>| {
        ContainerBasicInfo {
            container_id: if physical.is_empty() {
                String::new()
            } else {
                format!("{physical}-{id}")
            },
            container_name: container_name.clone(),
            container_ip: "10.42.0.9".into(),
            internal_port: 50051,
            external_port: 0,
            project_id: id.clone(),
            status: "running".into(),
            created_at: created,
            service_url: format!("http://{container_name}"),
            workload_uid: Some(workload.to_string()),
        }
    };
    let project_for = |container: ContainerBasicInfo| {
        let mut info = ProjectAndContainerInfo::new(id.clone());
        info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
        info.set_user_id(Some(format!("user-{id}")));
        info.set_container(Some(container));
        Arc::new(info)
    };
    let store = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .expect("connect");

    // 1. 注册原绑定 workload-0。
    store
        .insert_project_registration_durable(id.clone(), project_for(mk("pod-0", "wl-0", t0)))
        .await
        .expect("register original binding");
    assert!(
        store.wait_drained(Duration::from_secs(5)).await,
        "original drain"
    );
    let original = fetch_binding_row(&store, &container_name)
        .await
        .expect("original row present");
    assert_eq!(original.1.as_deref(), Some("wl-0"));

    // 2+3. 构造更早的观察 A（不登记）；登记创建更晚的 B → 合法带外换代。
    let late_a = mk("pod-a", "wl-a", t0 - chrono::Duration::hours(1));
    let b_info = mk("pod-b", "wl-b", t0 + chrono::Duration::hours(1));
    let outcome = store
        .insert_project_registration_durable(id.clone(), project_for(b_info.clone()))
        .await
        .expect("a newer out-of-band replacement must be admitted");
    assert_eq!(
        outcome,
        shared_types::persistence::PersistenceWriteOutcome::Committed
    );
    assert!(
        store.wait_drained(Duration::from_secs(5)).await,
        "replacement drain"
    );
    let row = fetch_binding_row(&store, &container_name)
        .await
        .expect("row after replacement");
    assert_eq!(row.1.as_deref(), Some("wl-b"), "binding must flip to B");

    // 4. 迟到登记 A：必须被拒绝，绑定不退回 A。
    let late = store
        .insert_project_registration_durable(id.clone(), project_for(late_a))
        .await;
    assert!(
        late.is_err(),
        "a stale observation older than the current binding must be rejected, got {late:?}"
    );
    let row = fetch_binding_row(&store, &container_name)
        .await
        .expect("row after late rejection");
    assert_eq!(
        row.0.as_deref(),
        Some(format!("pod-b-{id}").as_str()),
        "physical binding must stay on B"
    );
    assert_eq!(
        row.1.as_deref(),
        Some("wl-b"),
        "workload binding must stay on B"
    );

    // 5. B 未被墓碑退休：物理三元组查无墓碑，且 B 仍可重新登记（Committed）。
    let name_for_gen = container_name.clone();
    let b_generation = super::database::read(&store.database, move |tx| {
        let name = name_for_gen;
        Box::pin(async move {
            Ok(super::repo::fetch_container_registration(tx, &name)
                .await?
                .map(|row| row.container_generation))
        })
    })
    .await
    .expect("read B generation")
    .expect("B row present");
    assert!(
        !exact_tombstone_exists(
            &store,
            &container_name,
            &b_generation,
            &format!("pod-b-{id}"),
        )
        .await,
        "B must not be retired by a tombstone"
    );
    let recommit = store
        .insert_project_registration_durable(id.clone(), project_for(b_info))
        .await
        .expect("B must remain registrable");
    assert_eq!(
        recommit,
        shared_types::persistence::PersistenceWriteOutcome::Committed,
        "re-registering B is a plain rebase, not Superseded"
    );
    let _ = store.writer().flush_and_stop(Duration::from_secs(5)).await;
}

/// 回源自愈链（生产 2026-10-27 P0 的第二层防线）：副本镜像缺失该 user（节点
/// drain / 事件竞序）时，用户维度的查找 miss 经 `refresh_user_container` 回读
/// 权威行 + 受护栏的 project read-through hydrate 后重试命中；无容器的孤儿
/// project 行不遮蔽活跃容器。
#[tokio::test]
async fn user_lookup_miss_recovers_through_pg_read_through() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let id = format!("user-refresh-{}", uuid_suffix());
    let user_id = format!("user-{id}");

    let writer = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .unwrap();
    let mut info = ProjectAndContainerInfo::new(id.clone());
    info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
    info.set_user_id(Some(user_id.clone()));
    info.set_container(Some(container_for(&id)));
    writer
        .insert_with_session_durable(id.clone(), Arc::new(info), &format!("sess-{id}"))
        .await
        .unwrap();
    // 同 user 的无容器孤儿行（注册中途形态，遮蔽候选之一）。
    let orphan = format!("{id}-orphan");
    let mut orphan_info = ProjectAndContainerInfo::new(orphan.clone());
    orphan_info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
    orphan_info.set_user_id(Some(user_id.clone()));
    writer
        .insert_with_session_durable(
            orphan.clone(),
            Arc::new(orphan_info),
            &format!("sess-{orphan}"),
        )
        .await
        .unwrap();
    assert!(writer.wait_drained(Duration::from_secs(5)).await);

    // 独立镜像副本（无同步循环）。boot load 天然全量，因此对齐真实事故形态
    // （事件竞序把条目撕掉）：连接后显式移除该 user 的镜像条目再断言 miss。
    let reader = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .unwrap();
    reader.inner().remove(&id);
    reader.inner().remove(&orphan);
    use shared_types::ContainerLookup as _;
    assert!(
        reader
            .find_by_user_id(&user_id, &shared_types::ServiceType::ComputerAgentRunner)
            .is_none(),
        "a replica whose mirror lost the user must miss"
    );

    // 回源：活跃行 hydrate + 孤儿行不遮蔽 → 重试命中活跃容器。
    reader
        .refresh_user_container(&user_id, &shared_types::ServiceType::ComputerAgentRunner)
        .await
        .unwrap();
    let found = reader.find_by_user_id(&user_id, &shared_types::ServiceType::ComputerAgentRunner);
    assert_eq!(
        found.as_deref(),
        Some("10.42.0.9"),
        "the read-through resolves the user's live container past orphan rows"
    );
}

/// Codex 复审反例（P1）：候选查询不筛容器族时，同 user 的八个**更新的其他族**
/// 行会占满 LIMIT，Computer 行进不了候选集——回源"成功"却只恢复别的族，
/// VNC 查找仍 miss。修复后按族筛选，其他族行再多也不挤占候选。
#[tokio::test]
async fn user_refresh_candidates_are_scoped_to_the_requested_family() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let id = format!("family-refresh-{}", uuid_suffix());
    let user_id = format!("user-{id}");

    let writer = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .unwrap();
    // 一个 Computer 活跃行（目标）。
    let mut computer = ProjectAndContainerInfo::new(id.clone());
    computer.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
    computer.set_user_id(Some(user_id.clone()));
    computer.set_container(Some(container_for(&id)));
    writer
        .insert_with_session_durable(id.clone(), Arc::new(computer), &format!("sess-{id}"))
        .await
        .unwrap();
    // 八个更新的 Web 活跃行（created_at 更晚——注册顺序在后即可）。
    for index in 0..8 {
        let web_id = format!("{id}-web-{index}");
        let mut web = ProjectAndContainerInfo::new(web_id.clone());
        web.set_service_type(Some(shared_types::ServiceType::WebAgentRunner));
        web.set_user_id(Some(user_id.clone()));
        web.set_container(Some(container_for(&web_id)));
        writer
            .insert_with_session_durable(web_id.clone(), Arc::new(web), &format!("sess-{web_id}"))
            .await
            .unwrap();
    }
    assert!(writer.wait_drained(Duration::from_secs(5)).await);

    let reader = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .unwrap();
    use shared_types::ContainerLookup as _;
    // 撕掉 Computer 条目，保留 Web（模拟事故形态：仅目标族条目丢失）。
    reader.inner().remove(&id);
    assert!(
        reader
            .find_by_user_id(&user_id, &shared_types::ServiceType::ComputerAgentRunner)
            .is_none()
    );

    reader
        .refresh_user_container(&user_id, &shared_types::ServiceType::ComputerAgentRunner)
        .await
        .unwrap();
    let found = reader.find_by_user_id(&user_id, &shared_types::ServiceType::ComputerAgentRunner);
    assert_eq!(
        found.as_deref(),
        Some("10.42.0.9"),
        "newer other-family rows must not crowd out the family candidate set"
    );
}

/// Codex 建议固化的表锁反例（统一预算）：候选查询被 `LOCK TABLE projects` 阻塞时，
/// 回源必须在自身 durable 预算内返回超时错误，而不是等外层保护截断——修复前
/// 初始查询不限时，实测 100ms 预算 + 表锁 350ms 仍未结束。
#[tokio::test]
async fn user_refresh_budget_covers_a_blocked_candidate_query() {
    let Some(dsn) = test_dsn().await else {
        eprintln!("[skip] {DSN_ENV} not set");
        return;
    };
    let id = format!("budget-refresh-{}", uuid_suffix());
    let user_id = format!("user-{id}");

    let writer = PgStore::connect(&pg_config(&dsn), "test-ns".into(), "cluster.local".into())
        .await
        .unwrap();
    let mut info = ProjectAndContainerInfo::new(id.clone());
    info.set_service_type(Some(shared_types::ServiceType::ComputerAgentRunner));
    info.set_user_id(Some(user_id.clone()));
    info.set_container(Some(container_for(&id)));
    writer
        .insert_with_session_durable(id.clone(), Arc::new(info), &format!("sess-{id}"))
        .await
        .unwrap();
    assert!(writer.wait_drained(Duration::from_secs(5)).await);

    // 单连接 + 100ms durable 预算的受测副本。
    let reader = PgStore::connect(
        &PostgresConfig {
            max_connections: Some(1),
            min_connections: Some(1),
            durable_commit_timeout_ms: Some(100),
            ..pg_config(&dsn)
        },
        "test-ns".into(),
        "cluster.local".into(),
    )
    .await
    .unwrap();
    let database = reader.database.clone();
    let (started, ready) = tokio::sync::oneshot::channel::<()>();
    let (release, waiting) = tokio::sync::oneshot::channel::<()>();
    let held = tokio::spawn(async move {
        database
            .execute(move |mut db| async move {
                let mut tx = db.transaction().await?;
                toasty::sql::statement("LOCK TABLE projects IN ACCESS EXCLUSIVE MODE")
                    .exec(&mut tx)
                    .await?;
                started.send(()).unwrap();
                waiting.await.unwrap();
                tx.rollback().await?;
                Ok(())
            })
            .await
            .unwrap();
    });
    ready.await.unwrap();

    let started_at = std::time::Instant::now();
    let attempt = tokio::time::timeout(
        Duration::from_secs(2),
        reader.refresh_user_container(&user_id, &shared_types::ServiceType::ComputerAgentRunner),
    )
    .await;
    release.send(()).unwrap();
    held.await.unwrap();
    let elapsed = started_at.elapsed();
    let attempt = attempt.expect("the refresh must be covered by its own durable budget");
    let error = attempt.expect_err("a blocked candidate query must surface the budget timeout");
    assert!(
        error.to_string().contains("exceeded the durable deadline"),
        "{error:#}"
    );
    // 预算是 100ms：必须在预算量级内返回（宽松上界 600ms 覆盖调度与连接建立），
    // 绝不是等 2s 外层或无限期。
    assert!(
        elapsed < Duration::from_millis(600),
        "refresh returned in {elapsed:?}; the shared budget must bound it"
    );
}
