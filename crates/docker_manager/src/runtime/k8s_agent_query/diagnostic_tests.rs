use super::*;

fn pod(current: serde_json::Value, previous: serde_json::Value) -> Pod {
    serde_json::from_value(
        serde_json::json!({"metadata":{"uid":"pod-a"}, "status":{"containerStatuses":[{
            "name":"agent","image":"owned","imageID":"owned","ready":false,"restartCount":2,
            "state":current,"lastState":previous
        }]}}),
    )
    .unwrap()
}

#[test]
fn current_terminated_oom_is_observed_before_last_state_history() {
    let p = pod(
        serde_json::json!({"terminated":{"exitCode":137,"reason":"OOMKilled","finishedAt":"2026-10-22T14:00:00Z"}}),
        serde_json::json!({"terminated":{"exitCode":1,"reason":"Error","finishedAt":"2026-10-21T14:00:00Z"}}),
    );
    let d = pod_diagnostic(&p);
    assert!(d.is_oom());
    let evidence = d.evidence.unwrap();
    assert!(evidence.current_termination);
    assert_eq!(evidence.physical_id, "pod-a");
    assert_eq!(
        evidence.termination_finished_at.unwrap().timestamp(),
        1792677600
    );
}

#[test]
fn restarted_oom_is_historical_with_its_original_finish_time() {
    let p = pod(
        serde_json::json!({"running":{"startedAt":"2026-10-22T14:01:00Z"}}),
        serde_json::json!({"terminated":{"exitCode":137,"reason":"OOMKilled","finishedAt":"2026-10-22T14:00:00Z"}}),
    );
    let d = pod_diagnostic(&p);
    assert!(d.is_oom());
    let evidence = d.evidence.unwrap();
    assert!(!evidence.current_termination);
    assert_eq!(
        evidence.termination_finished_at.unwrap().timestamp(),
        1792677600
    );
}

#[test]
fn current_non_oom_termination_does_not_reuse_an_older_oom() {
    let p = pod(
        serde_json::json!({"terminated":{"exitCode":1,"reason":"Error"}}),
        serde_json::json!({"terminated":{"exitCode":137,"reason":"OOMKilled"}}),
    );
    let d = pod_diagnostic(&p);
    assert!(!d.is_oom());
    assert_eq!(d.last_terminate_reason.as_deref(), Some("Error"));
    assert!(d.evidence.unwrap().termination_finished_at.is_none());
}
