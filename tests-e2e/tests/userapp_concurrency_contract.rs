//! Deterministic concurrency contracts; not a substitute for real crash recovery.
use rcoder_e2e::common::report::JsonlReporter;

#[test]
fn userapp_concurrency_component_contract() {
    if !rcoder_e2e::common::require_context_or_skip() {
        return;
    }
    let report = JsonlReporter::begin(
        "userapp_concurrency_component_contract",
        "concurrency-component",
        serde_json::json!({"isolated": true}),
    );
    let result = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tools/concurrency_contract.py"
        ))
        .status();
    report.assert_hard(
        "Concurrency contract process completed",
        result.is_ok_and(|s| s.success()),
        "see concurrency-contract artifacts".into(),
    );
    let path = std::path::PathBuf::from(std::env::var_os("E2E_REPORT_DIR").unwrap())
        .join("concurrency-contract/assertions.json");
    let assertions: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(path).expect("Concurrency assertions file"))
            .expect("Concurrency assertions JSON");
    for item in assertions {
        report.assert_hard(
            item["name"].as_str().expect("assertion name"),
            item["ok"] == true,
            item["detail"].to_string(),
        );
    }
    assert!(report.finish(), "Concurrency lifecycle contract failed");
}
