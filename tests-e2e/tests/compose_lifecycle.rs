//! Physical Docker identities and data retention, independent of LLM credentials.
use rcoder_e2e::common::Env;

#[tokio::test]
async fn docker_deletion_identity_contract() {
    let scenario = "docker_deletion_identity_contract";
    let Some((_env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    let directory = std::env::var_os("E2E_REPORT_DIR").expect("run via strict E2E entry");
    let result = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tools/docker_lifecycle_contract.py"
        ))
        .status();
    report.assert_hard(
        "Docker lifecycle process completed",
        result.is_ok_and(|s| s.success()),
        "see docker-lifecycle artifacts".into(),
    );
    let path = std::path::PathBuf::from(directory).join("docker-lifecycle/assertions.json");
    let assertions: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(path).expect("Docker assertions file"))
            .expect("Docker assertions JSON");
    for item in assertions {
        report.assert_hard(
            item["name"].as_str().expect("assertion name"),
            item["ok"] == true,
            item["detail"].to_string(),
        );
    }
    assert!(report.finish(), "Docker lifecycle contract failed");
}
