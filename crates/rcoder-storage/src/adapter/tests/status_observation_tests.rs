//! 后台状态观察按物理代次条件应用的内存契约（D2）。
//!
//! 锁住的偏移敏感点：
//! - 物理身份（UID/workload）不一致即整体丢弃，不写状态也不动活动时钟
//! - 仅 active 观察刷新活动时钟；idle 只写状态
//! - 缺席项目不创建条目

use super::*;
use chrono::{Duration, Utc};

fn observation(
    uid: &str,
    workload: Option<&str>,
    active: bool,
) -> shared_types::AgentStatusObservation {
    shared_types::AgentStatusObservation {
        container_id: uid.to_string(),
        workload_uid: workload.map(str::to_string),
        project_generation: "gen-irrelevant-for-memory".to_string(),
        project_revision: 0,
        active,
    }
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

    assert!(adapter.apply_agent_status_observation("p1", &observation("c1-id", None, true)));

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

    assert!(adapter.apply_agent_status_observation("p2", &observation("c2-id", None, false)));

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
    assert!(!adapter.apply_agent_status_observation("p3", &observation("other-uid", None, false)));
    assert!(!adapter.apply_agent_status_observation(
        "p3",
        &observation("c3-id", Some("other-workload"), false)
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
    assert!(!adapter.apply_agent_status_observation("missing", &observation("uid", None, true)));
}
