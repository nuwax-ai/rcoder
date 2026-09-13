//! Real rcoder Docker SIGKILL and Docker crash restart quarantine contracts.
use rcoder_e2e::common::report::JsonlReporter;

#[test]
fn docker_runtime_crash_recovery_contract() {
    if !rcoder_e2e::common::require_context_or_skip() {
        return;
    }
    let report = JsonlReporter::begin(
        "docker_runtime_crash_recovery_contract",
        "docker-crash",
        serde_json::json!({"isolated": true}),
    );
    let result = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tools/docker_crash_contract.py"
        ))
        .status();
    report.assert_hard(
        "Docker crash contract process completed",
        result.is_ok_and(|s| s.success()),
        "see docker-crash artifacts".into(),
    );
    let path = std::path::PathBuf::from(std::env::var_os("E2E_REPORT_DIR").unwrap())
        .join("docker-crash/assertions.json");
    let assertions: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(path).expect("Docker crash assertions file"))
            .expect("Docker crash assertions JSON");
    for item in assertions {
        report.assert_hard(
            item["name"].as_str().expect("assertion name"),
            item["ok"] == true,
            item["detail"].to_string(),
        );
    }
    assert!(report.finish(), "Docker crash lifecycle contract failed");
}
