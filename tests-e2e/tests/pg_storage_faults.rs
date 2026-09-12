//! Real PostgreSQL lifecycle contracts; independent of LLM and platform storage mode.
use rcoder_e2e::common::report::JsonlReporter;

#[test]
fn pg_storage_lifecycle_contract() {
    if std::env::var_os("E2E_REPORT_DIR").is_none() {
        assert_ne!(
            std::env::var("E2E_STRICT").as_deref(),
            Ok("1"),
            "strict PG suite requires report directory"
        );
        return;
    }
    let report = JsonlReporter::begin(
        "pg_storage_lifecycle_contract",
        "postgres17",
        serde_json::json!({"isolated": true}),
    );
    let result = std::process::Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tools/pg_contract.py"))
        .status();
    report.assert_hard(
        "PG contract process completed",
        result.is_ok_and(|s| s.success()),
        "see pg-contract artifacts".into(),
    );
    let path = std::path::PathBuf::from(std::env::var_os("E2E_REPORT_DIR").unwrap())
        .join("pg-contract/assertions.json");
    let assertions: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(path).expect("PG assertions file"))
            .expect("PG assertions JSON");
    for item in assertions {
        report.assert_hard(
            item["name"].as_str().expect("assertion name"),
            item["ok"] == true,
            item["detail"].to_string(),
        );
    }
    assert!(report.finish(), "PG lifecycle contract failed");
}
