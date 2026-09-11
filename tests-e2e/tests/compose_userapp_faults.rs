//! Real runtime Docker A/B and failure contracts, independent of LLM credentials.
use rcoder_e2e::common::{Env, report::JsonlReporter};

#[tokio::test]
async fn userapp_hot_deployment_builtin_contract() {
    run_scenario("userapp_hot_deployment_builtin_contract", "builtin").await;
}

#[tokio::test]
async fn userapp_hot_deployment_supervisord_contract() {
    run_scenario("userapp_hot_deployment_supervisord_contract", "supervisord").await;
}

async fn run_scenario(scenario: &str, engine: &str) {
    let Some((_env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    run_contract(&report, engine);
    assert!(report.finish(), "hot deployment contract failed");
}

fn run_contract(report: &JsonlReporter, engine: &str) {
    let Some(directory) = std::env::var_os("E2E_REPORT_DIR") else {
        report.assert_hard(
            "strict runner report directory",
            false,
            "run via make test-e2e".into(),
        );
        return;
    };
    let result = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tools/hot_contract.py"
        ))
        .env("E2E_APP_CLI_ENGINE", engine)
        .status();
    report.assert_hard(
        "Docker contract process completed",
        result.is_ok_and(|s| s.success()),
        "see hot-contract artifacts".into(),
    );
    let path = std::path::PathBuf::from(directory).join("hot-contract/assertions.json");
    let assertions = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<serde_json::Value>>(&b).ok());
    match assertions {
        Some(assertions) => {
            for item in assertions {
                report.assert_hard(
                    item["name"].as_str().unwrap_or("unnamed contract"),
                    item["ok"] == true,
                    item["detail"].to_string(),
                );
            }
        }
        None => {
            report.assert_hard(
                "contract assertions present",
                false,
                path.display().to_string(),
            );
        }
    }
}
