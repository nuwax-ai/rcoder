//! 后台状态观察按物理代次条件应用的内存契约（D2）。
//!
//! 锁住的偏移敏感点：
//! - 物理身份（UID/workload）不一致即整体丢弃，不写状态也不动活动时钟
//! - 仅 active 观察刷新活动时钟；idle 只写状态
//! - 缺席项目不创建条目

use super::*;
use chrono::{Duration, Utc};

fn observation(
    adapter: &ProjectAdapter,
    project: &str,
    uid: &str,
    workload: Option<&str>,
    active: bool,
) -> shared_types::AgentStatusObservation {
    let mut value = shared_types::AgentStatusObservation::capture(
        &adapter
            .get(project)
            .unwrap_or_else(|| Arc::new(create_test_info_with_container(project, "absent"))),
        active,
    )
    .unwrap();
    value.container_id = uid.into();
    value.workload_uid = workload.map(str::to_owned);
    value
}

#[test]
fn same_physical_active_observation_updates_status_and_activity() {
    let adapter = make_adapter();
    let mut info = create_test_info_with_container("p1", "c1");
    let stale = Utc::now() - Duration::hours(2);
    info.set_timestamps(stale, stale);
    adapter
        .insert("p1".to_string(), Arc::new(info))
        .expect("插入");

    assert!(
        adapter.apply_agent_status_observation(
            "p1",
            &observation(&adapter, "p1", "c1-id", None, true)
        )
    );

    let current = adapter.get("p1").expect("注册仍在");
    assert_eq!(current.status(), Some(&shared_types::AgentStatus::Active));
    assert!(
        current.last_activity() > stale,
        "active 观察必须刷新活动时钟"
    );
}

#[test]
fn same_physical_idle_observation_updates_status_only() {
    let adapter = make_adapter();
    let mut info = create_test_info_with_container("p2", "c2");
    let stale = Utc::now() - Duration::hours(2);
    info.set_timestamps(stale, stale);
    adapter
        .insert("p2".to_string(), Arc::new(info))
        .expect("插入");

    assert!(
        adapter.apply_agent_status_observation(
            "p2",
            &observation(&adapter, "p2", "c2-id", None, false)
        )
    );

    let current = adapter.get("p2").expect("注册仍在");
    assert_eq!(current.status(), Some(&shared_types::AgentStatus::Idle));
    assert_eq!(
        current.last_activity(),
        stale,
        "idle 观察不得刷新活动时钟（否则闲置回收被永久抑制）"
    );
}

#[test]
fn replaced_physical_identity_discards_observation_entirely() {
    let adapter = make_adapter();
    let mut info = create_test_info_with_container("p3", "c3");
    let stale = Utc::now() - Duration::hours(2);
    info.set_timestamps(stale, stale);
    info.set_status(Some(shared_types::AgentStatus::Active));
    adapter
        .insert("p3".to_string(), Arc::new(info))
        .expect("插入");

    // 换代后的新 UID（或同 UID 不同 workload）迟到观察：全部丢弃
    assert!(!adapter.apply_agent_status_observation(
        "p3",
        &observation(&adapter, "p3", "other-uid", None, false)
    ));
    assert!(!adapter.apply_agent_status_observation(
        "p3",
        &observation(&adapter, "p3", "c3-id", Some("other-workload"), false)
    ));

    let current = adapter.get("p3").expect("注册仍在");
    assert_eq!(
        current.status(),
        Some(&shared_types::AgentStatus::Active),
        "旧观察不得覆盖新注册状态"
    );
    assert_eq!(
        current.last_activity(),
        stale,
        "旧观察不得刷新新代次活动时钟"
    );
}

#[test]
fn absent_project_rejects_observation() {
    let adapter = make_adapter();
    assert!(!adapter.apply_agent_status_observation(
        "missing",
        &observation(&adapter, "missing", "uid", None, true)
    ));
}

#[test]
fn stale_project_epoch_on_same_physical_container_is_rejected() {
    let adapter = make_adapter();
    let original = create_test_info_with_container("same-physical-project", "same-container");
    let original_identity = original.persistence_identity().clone();
    let basic = original.container_info().unwrap();
    adapter
        .insert(original.project_id().into(), Arc::new(original.clone()))
        .unwrap();
    let mut successor = original;
    let mut identity = successor.persistence_identity().clone();
    identity.generation = "successor-project-epoch".into();
    successor.set_persistence_identity(identity);
    successor.set_status(Some(shared_types::AgentStatus::Active));
    adapter
        .insert(successor.project_id().into(), Arc::new(successor))
        .unwrap();
    let stale = shared_types::AgentStatusObservation {
        container_name: basic.container_name,
        container_generation: original_identity
            .container
            .as_ref()
            .map(|value| value.generation.clone()),
        container_revision: original_identity
            .container
            .as_ref()
            .map(|value| value.revision),
        container_id: basic.container_id,
        workload_uid: basic.workload_uid,
        project_generation: original_identity.generation,
        project_revision: original_identity.revision,
        active: false,
    };
    assert!(
        !adapter.apply_agent_status_observation("same-physical-project", &stale),
        "an unchanged Pod UID is not authority over a recreated project"
    );
    assert_eq!(
        adapter.get("same-physical-project").unwrap().status(),
        Some(&shared_types::AgentStatus::Active)
    );
}
