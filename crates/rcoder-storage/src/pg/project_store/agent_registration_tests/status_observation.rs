//! 后台状态观察按**捕获代次**持久化的 PG 契约（D2）。
//!
//! 锁住的偏移敏感点：
//! - 持久化 op 携带观察捕获的 generation/revision（不是写入时的当前值）：
//!   跨副本镜像滞后时，SQL CAS（generation + row_revision）把迟到观察判
//!   Superseded，PG 行不落旧状态
//! - 同代次观察正常落库（含活动时钟 touch 的代次条件）
//!
//! 注：两个场景分属独立测试——UpdateAgentStatus 走节流队列，同 project
//! 短窗口内的第二次入队会被合并丢弃，不能在同测试内先后断言两态。

use super::*;

async fn agent_status_text(observer: &crate::db::owner::DatabaseOwner, id: &str) -> Option<String> {
    test_support::optional_text(
        observer,
        "SELECT coalesce(agent_status_json::text, '') FROM projects WHERE project_id=$1",
        id,
    )
    .await
}

/// 跨副本竞态：观察捕获旧代次（镜像 UID 未变、放行），SQL CAS 必须拒绝。
#[tokio::test]
async fn stale_generation_observation_is_superseded_in_pg() {
    let Some(dsn) = test_support::test_dsn().await else {
        eprintln!("skipping: RCODER_PG_TEST_DSN not set");
        return;
    };
    let suffix = test_support::uuid_suffix();
    let id = format!("status-observation-stale-{suffix}");
    let physical = basic(&id);
    let observer = test_support::database(&dsn).await;

    let writer = store(&dsn).await;
    writer
        .insert_project_registration_durable(id.clone(), project(&id, &id, &physical))
        .await
        .unwrap();
    assert!(writer.wait_drained(Duration::from_secs(5)).await);

    // 观察方（peer）从自己的镜像捕获旧代次身份
    let peer = store(&dsn).await;
    let captured_registration = peer.get(&id).unwrap();
    let captured_basic = captured_registration.container_info().unwrap();
    let captured_identity = captured_registration.persistence_identity().clone();

    // 写侧完成一次受控换代（新 Pod UID），PG 行进入新代次
    let replacement_evidence = evidence(&physical);
    let expected = frozen(&writer.get(&id).unwrap());
    assert_eq!(
        writer
            .replace_agent_container_registration_durable(&expected, &replacement_evidence)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert!(writer.wait_drained(Duration::from_secs(5)).await);

    // peer 镜像仍持旧 UID（滞后副本的正常形态）：镜像层按 UID 放行；
    // 正确性由持久化层的捕获代次 CAS 承担
    let stale_observation = shared_types::AgentStatusObservation {
        container_id: captured_basic.container_id.clone(),
        workload_uid: captured_basic.workload_uid.clone(),
        project_generation: captured_identity.generation.clone(),
        project_revision: captured_identity.revision,
        active: false,
    };
    assert!(peer.apply_agent_status_observation(&id, &stale_observation));
    assert!(peer.wait_drained(Duration::from_secs(5)).await);
    let status = agent_status_text(&observer, &id).await;
    assert_eq!(
        status.as_deref(),
        Some(""),
        "捕获代次已被换代取代：SQL CAS 必须判 Superseded，不得落旧状态"
    );
}

/// 同代次观察正常落库；换代后的旧代次重放不得覆盖。
#[tokio::test]
async fn current_generation_observation_persists_and_old_generation_never_overwrites() {
    let Some(dsn) = test_support::test_dsn().await else {
        eprintln!("skipping: RCODER_PG_TEST_DSN not set");
        return;
    };
    let suffix = test_support::uuid_suffix();
    let id = format!("status-observation-fresh-{suffix}");
    let physical = basic(&id);
    let observer = test_support::database(&dsn).await;

    let store_handle = store(&dsn).await;
    store_handle
        .insert_project_registration_durable(id.clone(), project(&id, &id, &physical))
        .await
        .unwrap();
    assert!(store_handle.wait_drained(Duration::from_secs(5)).await);

    // 同代次（当前）观察：active=true 落库
    let current = store_handle.get(&id).unwrap();
    let current_basic = current.container_info().unwrap();
    let current_identity = current.persistence_identity().clone();
    let fresh_observation = shared_types::AgentStatusObservation {
        container_id: current_basic.container_id.clone(),
        workload_uid: current_basic.workload_uid.clone(),
        project_generation: current_identity.generation.clone(),
        project_revision: current_identity.revision,
        active: true,
    };
    assert!(store_handle.apply_agent_status_observation(&id, &fresh_observation));
    assert!(store_handle.wait_drained(Duration::from_secs(5)).await);
    let status = agent_status_text(&observer, &id).await;
    assert!(
        status
            .as_deref()
            .is_some_and(|value| value.contains("Active")),
        "同代次观察必须落库：{status:?}"
    );

    // 换代后重放旧代次观察（物理 UID 相同、代次过期）：镜像放行，
    // SQL CAS 拒绝——PG 行保持新代次写入的 Active
    let stale_generation_observation = shared_types::AgentStatusObservation {
        container_id: current_basic.container_id.clone(),
        workload_uid: current_basic.workload_uid.clone(),
        project_generation: format!("superseded-generation-{suffix}"),
        project_revision: 0,
        active: false,
    };
    assert!(store_handle.apply_agent_status_observation(&id, &stale_generation_observation));
    assert!(store_handle.wait_drained(Duration::from_secs(5)).await);
    let status = agent_status_text(&observer, &id).await;
    assert!(
        status
            .as_deref()
            .is_some_and(|value| value.contains("Active")),
        "旧代次迟到观察不得覆盖新代次状态：{status:?}"
    );
}
