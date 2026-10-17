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

#[tokio::test]
async fn userapp_worker_builtin_contract() {
    worker_scenario("userapp_worker_builtin_contract", "builtin").await;
}

#[tokio::test]
async fn userapp_worker_supervisord_contract() {
    worker_scenario("userapp_worker_supervisord_contract", "supervisord").await;
}

async fn worker_scenario(scenario: &str, engine: &str) {
    let Some((_env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    run_contract(&report, engine, "worker_contract.py", "worker-contract");
    assert!(report.finish(), "worker contract failed");
}

async fn run_scenario(scenario: &str, engine: &str) {
    let Some((_env, report)) = Env::compose_or_skip(scenario, "compose").await else {
        return;
    };
    run_contract(&report, engine, "hot_contract.py", "hot-contract");
    assert!(report.finish(), "hot deployment contract failed");
}

fn run_contract(report: &JsonlReporter, engine: &str, script: &str, artifacts: &str) {
    let Some(directory) = std::env::var_os("E2E_REPORT_DIR") else {
        report.assert_hard(
            "strict runner report directory",
            false,
            "run via make test-e2e".into(),
        );
        return;
    };
    let result = std::process::Command::new("python3")
        .arg(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tools")
                .join(script),
        )
        .env("E2E_APP_CLI_ENGINE", engine)
        .status();
    report.assert_hard(
        "Docker contract process completed",
        result.is_ok_and(|s| s.success()),
        format!("see {artifacts} artifacts"),
    );
    let path = std::path::PathBuf::from(directory)
        .join(artifacts)
        .join("assertions.json");
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

/// Small real RCoder -> app-runtime A/B lifecycle, independent of chat/LLM.
/// The isolated controller and paired build receipt are explicit prerequisites.
#[tokio::test]
async fn userapp_prod_readiness_contract() {
    if !rcoder_e2e::common::require_context_or_skip() {
        return;
    }
    let report = JsonlReporter::begin(
        "userapp_prod_readiness_contract",
        "compose",
        serde_json::json!({"fixture": "isolated-loopback-prod", "llm": false}),
    );
    let result = (|| -> Result<(), String> {
        let required = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| format!("missing required isolated fixture input: {name}"))
        };
        let directory =
            std::path::PathBuf::from(required("E2E_REPORT_DIR")?).join("prod-readiness-contract");
        std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let path = directory.join("assertions.json");
        let status = std::process::Command::new("python3")
            .arg(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tools/prod_readiness_contract.py"),
            )
            .arg("--build-receipt")
            .arg(required("E2E_PROD_BUILD_RECEIPT")?)
            .arg("--report")
            .arg(&path)
            .status()
            .map_err(|error| format!("cannot launch prod fixture: {error}"))?;
        report.assert_hard(
            "prod contract process completed",
            status.success(),
            status.to_string(),
        );
        let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        let checks = value["checks"]
            .as_array()
            .ok_or("prod report checks missing")?;
        for check in checks {
            let name = check["name"].as_str().ok_or("prod check name missing")?;
            report.assert_hard(name, check["ok"] == true, check["detail"].to_string());
        }
        report.assert_hard(
            "prod contract reports full success",
            value["success"] == true && value["full_owned_acceptance"] == true,
            value["error"].to_string(),
        );
        report.assert_hard(
            "prod cleanup confirms retained volumes",
            value["cleanup"]["captured_containers_removed"] == true
                && value["cleanup"]["volumes_removed"] == false,
            value["cleanup"].to_string(),
        );
        Ok(())
    })();
    if let Err(error) = result {
        report.assert_hard("prod contract evidence readable", false, error);
    }
    assert!(report.finish(), "prod readiness contract failed");
}

/// Real Stopped failure and same-operation recovery; no shared services or AI.
#[tokio::test]
async fn userapp_compute_stopped_failure_recovery_contract() {
    compute_failure_scenario(
        "userapp_compute_stopped_failure_recovery_contract",
        "stopped_read",
    )
    .await;
}

/// Docker has completed Stop, but its transport reply is held until the client timeout.
#[tokio::test]
async fn userapp_compute_stop_timeout_recovery_contract() {
    compute_failure_scenario(
        "userapp_compute_stop_timeout_recovery_contract",
        "stop_reply_timeout",
    )
    .await;
}

async fn compute_failure_scenario(scenario: &str, mode: &str) {
    if !rcoder_e2e::common::require_context_or_skip() {
        return;
    }
    let report = JsonlReporter::begin(
        scenario,
        "compose",
        serde_json::json!({"fixture": "isolated-compute-failure", "fault_mode": mode, "llm": false}),
    );
    let result = (|| -> Result<(), String> {
        let directory = std::env::var_os("E2E_REPORT_DIR")
            .map(std::path::PathBuf::from)
            .ok_or("run via the strict E2E launcher")?;
        let status = std::process::Command::new("python3")
            .arg(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tools/compute_failure_contract.py"),
            )
            .env(
                "E2E_COMPUTE_APP_ENTRY_PORT",
                shared_types::APP_ENTRY_PORT.to_string(),
            )
            .env("E2E_COMPUTE_FAILURE_MODE", mode)
            .status()
            .map_err(|error| format!("cannot launch compute fixture: {error}"))?;
        report.assert_hard(
            "compute failure contract process completed",
            status.success(),
            status.to_string(),
        );
        let path = directory.join("compute-failure-contract/result.json");
        let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        for check in value["checks"].as_array().ok_or("compute checks missing")? {
            report.assert_hard(
                check["name"].as_str().ok_or("compute check name missing")?,
                check["ok"] == true,
                check["detail"].to_string(),
            );
        }
        report.assert_hard(
            "compute failure contract reports full success",
            value["success"] == true,
            "see private compute-failure-contract diagnostics".into(),
        );
        Ok(())
    })();
    if let Err(error) = result {
        report.assert_hard("compute failure evidence readable", false, error);
    }
    assert!(report.finish(), "compute failure recovery failed: {mode}");
}
