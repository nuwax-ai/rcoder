//! Native SIGKILL process integration; not Docker crash-recovery evidence.
use rcoder_e2e::common::report::JsonlReporter;

#[test]
fn native_terminal_release_crash_contract() {
    if !rcoder_e2e::common::require_context_or_skip() {
        return;
    }
    let report = JsonlReporter::begin(
        "native_terminal_release_crash_contract",
        "native-process",
        serde_json::json!({"isolated": true}),
    );
    let result = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tools/native_crash_contract.py"
        ))
        .status();
    report.assert_hard(
        "Native contract process completed",
        result.is_ok_and(|s| s.success()),
        "see native-crash artifacts".into(),
    );
    let path = std::path::PathBuf::from(std::env::var_os("E2E_REPORT_DIR").unwrap())
        .join("native-crash/assertions.json");
    let assertions: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(path).expect("Native assertions file"))
            .expect("Native assertions JSON");
    for item in assertions {
        report.assert_hard(
            item["name"].as_str().expect("assertion name"),
            item["ok"] == true,
            item["detail"].to_string(),
        );
    }
    assert!(report.finish(), "Native lifecycle contract failed");
}
