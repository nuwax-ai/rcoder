//! Real isolated Compose SQLite recreation; independent of LLM credentials.
use rcoder_e2e::common::report::JsonlReporter;

#[test]
fn sqlite_compose_recreation_contract() {
    if !rcoder_e2e::common::require_context_or_skip() {
        return;
    }
    let report = JsonlReporter::begin(
        "sqlite_compose_recreation_contract",
        "sqlite-compose",
        serde_json::json!({"isolated": true}),
    );
    let result = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tools/sqlite_runtime_contract.py"
        ))
        .status();
    report.assert_hard(
        "SQLite contract process completed",
        result.is_ok_and(|s| s.success()),
        "see sqlite-runtime artifacts".into(),
    );
    let path = std::path::PathBuf::from(std::env::var_os("E2E_REPORT_DIR").unwrap())
        .join("sqlite-runtime/assertions.json");
    let assertions: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(path).expect("SQLite assertions file"))
            .expect("SQLite assertions JSON");
    for item in assertions {
        report.assert_hard(
            item["name"].as_str().expect("assertion name"),
            item["ok"] == true,
            item["detail"].to_string(),
        );
    }
    assert!(report.finish(), "SQLite lifecycle contract failed");
}
