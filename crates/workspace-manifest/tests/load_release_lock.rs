//! Golden 回归测试：永久保护"最老支持的版本仍能经 [`workspace_manifest::load_release_lock`]
//! 正常加载"。
//!
//! 任何破坏当前反序列化（或未来迁移链）的改动若让本 fixture 无法加载，本测试立即失败。
//! 引入新版本时，新增 v{N} fixture + 对应 v{N}→current 快照断言，**不要删除本 v1 测试**。

use std::fs;

use workspace_manifest::{LoadError, ReleaseLock, SCHEMA_VERSION, load_release_lock};

const FIXTURE: &str = "tests/fixtures/lock_v1.toml";

fn load_fixture() -> ReleaseLock {
    let content = fs::read_to_string(FIXTURE).expect("read fixture");
    load_release_lock(&content).expect("load v1 fixture")
}

#[test]
fn loads_v1_fixture_to_current_schema() {
    let lock = load_fixture();
    assert_eq!(lock.schema_version, SCHEMA_VERSION);
    assert_eq!(lock.workspace_name, "demo");
    assert_eq!(lock.release_id, "01923a5f8c217abc9def0123456789ab");
    // bridge_service 是加性字段（Option + serde(default)）：老 lock 缺失会填 None，
    // 本 fixture 显式写了，验证加性路径在当前版本下正确解析。
    assert_eq!(lock.bridge_service.as_deref(), Some("backend-go"));
}

#[test]
fn parses_service_port_identity_and_routes() {
    let lock = load_fixture();
    let service = lock.services.first().expect("at least one service");
    assert_eq!(service.service_id, "backend-go");
    assert_eq!(service.port, 4100);
    assert!(service.enabled);
    assert_eq!(service.health.readiness_path, "/ready");
    assert_eq!(service.proxy.as_ref().expect("proxy").path, "/api/go/");
    assert!(service.proxy.as_ref().unwrap().strip_prefix);
    assert_eq!(service.logs.len(), 1);
    assert_eq!(service.logs[0].id, "application");
    assert_eq!(service.run.command, vec!["./server".to_owned()]);
}

#[test]
fn preserves_deny_unknown_fields_fail_fast() {
    // load_release_lock 先用宽松的 VersionPeek 取版本，再对当前型做严格反序列化。
    // deny_unknown_fields 的 fail-fast 必须在严格反序列化这一步保留。
    let content = fs::read_to_string(FIXTURE).expect("read fixture");
    let with_unknown = content.replacen(
        "schema_version = 1",
        "schema_version = 1\nunknown_top_level_field = true",
        1,
    );
    assert!(
        matches!(load_release_lock(&with_unknown), Err(LoadError::Parse(_))),
        "unknown field must be rejected (deny_unknown_fields), not silently ignored"
    );
}

#[test]
fn rejects_newer_than_known_schema_version() {
    let content = fs::read_to_string(FIXTURE).expect("read fixture");
    let newer = content.replacen("schema_version = 1", "schema_version = 99", 1);
    match load_release_lock(&newer) {
        Err(LoadError::NewerThanKnown { got: 99, known }) => assert_eq!(known, SCHEMA_VERSION),
        other => panic!("expected NewerThanKnown{{got:99}}, got {other:?}"),
    }
}

#[test]
fn rejects_unknown_old_schema_version() {
    // 当前 SCHEMA_VERSION=1，没有比 1 更老的注册版本，故 v=0 落到 UnknownVersion。
    // （未来升到 v{N} 后，< N 的已注册历史版本会走迁移分支；此处验证"既非当前、又非未来"
    // 的版本被明确拒绝，而非静默当成当前。）
    let content = fs::read_to_string(FIXTURE).expect("read fixture");
    let zero = content.replacen("schema_version = 1", "schema_version = 0", 1);
    match load_release_lock(&zero) {
        Err(LoadError::UnknownVersion(0)) => {}
        other => panic!("expected UnknownVersion(0), got {other:?}"),
    }
}

#[test]
fn rejects_release_lock_with_no_services() {
    // services = [] 必须在 [pingap] 之前（TOML：进入子表后键归子表）。
    let minimal = "\
schema_version = 1
release_id = \"x\"
workspace_name = \"x\"
minimum_app_cli_version = \"0.1.0\"
runtime_image_digest = \"img\"
services = []

[pingap]
mode = \"managed\"
version = \"0.13.8\"
commit = \"c\"
";
    match load_release_lock(minimal) {
        Err(LoadError::Invariant(message)) => assert!(message.contains("no services"), "{message}"),
        other => panic!("expected Invariant (empty services), got {other:?}"),
    }
}
