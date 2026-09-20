//! Conversion boundaries shared by the PostgreSQL and Turso implementations.
use super::*;

fn application_fixture() -> models::Application {
    models::Application {
        app_id: "codecapp".into(),
        lifecycle_id: "life1".into(),
        lifecycle_epoch: 1,
        lifecycle_state: "active".into(),
        metadata_revision: 1,
        name: None,
        tenant_id: None,
        space_id: None,
        recycle_enabled: None,
        wake_on_traffic: None,
        idle_timeout_seconds: None,
        created_at_us: 0,
        updated_at_us: 0,
    }
}
fn slots() -> models::ActiveOperations {
    models::ActiveOperations {
        app_id: "codecapp".into(),
        lifecycle_id: "life1".into(),
        dev_operation_id: None,
        prod_operation_id: None,
        application_operation_id: None,
    }
}
fn operation_fixture() -> models::Operation {
    models::Operation {
        operation_id: "operation1".into(),
        app_id: "codecapp".into(),
        lifecycle_id: "life1".into(),
        kind: "start".into(),
        scope: "prod".into(),
        state: "pending".into(),
        revision: 1,
        origin_request_id: None,
        request_fingerprint: "a".repeat(64),
        executor_id: None,
        step: "accepted".into(),
        error_code: None,
        error_message: None,
        payload_version: 1,
        command_json: None,
        admitted_metadata_json: None,
        runtime_policy_on_success_json: None,
        checkpoint_json: "{\"large\":18446744073709551615,\"nested\":[null,true,\"中文\"]}".into(),
        created_at_us: 0,
        updated_at_us: 0,
        terminal_at_us: None,
    }
}

#[test]
fn integer_limits_are_preserved_and_overflow_is_rejected() {
    let mut row = application_fixture();
    row.lifecycle_epoch = i64::MAX;
    row.metadata_revision = i64::MAX;
    row.idle_timeout_seconds = Some(i64::MAX);
    let mut record = application(row, slots()).unwrap();
    let encoded = application_row(&record, 0).unwrap();
    assert_eq!(encoded.lifecycle_epoch, i64::MAX);
    assert_eq!(encoded.metadata_revision, i64::MAX);
    assert_eq!(encoded.idle_timeout_seconds, Some(i64::MAX));
    for value in [i64::MAX as u64 + 1, u64::MAX] {
        record.runtime_policy.idle_timeout_seconds = Some(value);
        assert!(application_row(&record, 0).is_err());
    }
    for value in [-1, i64::MIN] {
        let mut row = application_fixture();
        row.idle_timeout_seconds = Some(value);
        assert!(application(row, slots()).is_err());
    }
    for value in [0, -1, i64::MIN] {
        let mut row = application_fixture();
        row.metadata_revision = value;
        assert!(application(row, slots()).is_err());
    }
}

#[test]
fn microseconds_roundtrip_and_out_of_range_dates_fail() {
    for micros in [-1_000_001, -1, 0, 1, 1_000_001] {
        let mut row = application_fixture();
        row.created_at_us = micros;
        let record = application(row, slots()).unwrap();
        assert_eq!(
            application_row(&record, micros).unwrap().created_at_us,
            micros
        );
    }
    for micros in [i64::MIN, i64::MAX] {
        let mut row = application_fixture();
        row.created_at_us = micros;
        assert!(application(row, slots()).is_err());
        let mut row = operation_fixture();
        row.created_at_us = micros;
        assert!(operation(row).is_err());
    }
}

#[test]
fn json_large_integers_roundtrip_and_corrupt_payloads_fail() {
    let record = operation(operation_fixture()).unwrap();
    assert_eq!(record.checkpoint["large"].as_u64(), Some(u64::MAX));
    let encoded = operation_row(&record, 0, None).unwrap();
    assert_eq!(operation(encoded).unwrap().checkpoint, record.checkpoint);
    for field in ["checkpoint", "command", "metadata", "policy"] {
        let mut row = operation_fixture();
        match field {
            "checkpoint" => row.checkpoint_json = "{".into(),
            "command" => row.command_json = Some("{".into()),
            "metadata" => row.admitted_metadata_json = Some("{".into()),
            _ => row.runtime_policy_on_success_json = Some("{".into()),
        }
        assert!(operation(row).is_err(), "{field}");
    }
}
